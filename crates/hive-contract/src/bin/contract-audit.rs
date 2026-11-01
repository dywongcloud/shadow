//! Live witness harness for the contract/execution layer.
//!
//! Not a test file and not a mock: it runs the real contract, the real executor and the real replay
//! auditor against a REAL platform snapshot (`$HIVE_DATA/state.json` copied off a fleet node), and
//! prints what happened. Every claim the design makes has a probe here that could falsify it.
//!
//! Usage: `contract-audit --state <state.json> [--rounds N]`
//!
//! Connection credentials in the snapshot are never read and never printed.

use hive_contract::contract::{AuthPolicy, Caller, Contract, Effect, OpError, UNTAGGED_TENANT};
use hive_contract::contracts::managed_database::managed_db_contract;
use hive_contract::engine::{Engine, DEFAULT_SHARDS};
use hive_contract::exec::{Executor, Index};
use hive_contract::oplog::{Op, OpLog};
use hive_contract::{audit, index_hash};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

/// One probe of a demonstrated defect. The attack that exposed the defect, re-run.
fn check(failures: &mut usize, ok: bool, label: &str, detail: String) {
    if ok {
        println!("PASS {label}: {detail}");
    } else {
        println!("FAIL {label}: {detail}");
        *failures += 1;
    }
}

/// A managed database record as it appears in the platform snapshot. Only the non-secret fields are
/// read: `connection` (passwords, connection strings) is deliberately absent from this struct.
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
    region: String,
}

#[derive(serde::Deserialize)]
struct Snapshot {
    #[serde(default)]
    databases: Vec<RealDb>,
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

/// The platform's gate already ran and returned "this tenant owns this target". That is the ONLY
/// way a `Caller` with `owns_target: true` is built anywhere in this crate.
fn owner(tenant: &str) -> Caller {
    Caller {
        tenant: tenant.to_string(),
        actor: "platform:require_project".to_string(),
        owns_target: true,
    }
}

#[tokio::main]
async fn main() {
    let path = arg("state", "/tmp/vitra-real-state.json");
    let rounds: usize = arg("rounds", "20").parse().unwrap_or(20);

    let raw = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("FAIL cannot read {path}: {e}");
            std::process::exit(2);
        }
    };
    let snap: Snapshot = match serde_json::from_slice(&raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("FAIL cannot parse {path}: {e}");
            std::process::exit(2);
        }
    };
    let dbs: Vec<&RealDb> = snap.databases.iter().filter(|d| !d.id.is_empty()).collect();
    println!("== REAL INPUT ==");
    println!("state file      : {path} ({} bytes)", raw.len());
    println!("databases found : {}", dbs.len());
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_region: BTreeMap<String, usize> = BTreeMap::new();
    let mut teams: BTreeMap<String, usize> = BTreeMap::new();
    for d in &dbs {
        *by_kind.entry(d.kind.clone()).or_default() += 1;
        *by_region.entry(d.region.clone()).or_default() += 1;
        *teams.entry(if d.team.is_empty() {
            UNTAGGED_TENANT.to_string()
        } else {
            d.team.clone()
        })
        .or_default() += 1;
    }
    println!("by kind         : {by_kind:?}");
    println!("by region       : {by_region:?}");
    println!("distinct tenants: {}", teams.len());
    if dbs.is_empty() {
        eprintln!("FAIL no databases in snapshot; nothing to witness");
        std::process::exit(3);
    }

    let contract = managed_db_contract();
    println!("\n== CONTRACT ==");
    println!("id             : {}", contract.id);
    println!(
        "declared kinds : {:?}",
        contract.ops.keys().collect::<Vec<_>>()
    );

    // ---- P1: the one contract shape that would let an unauthenticated proposer write tenant data
    // must be unrepresentable, not merely rejected later. ----
    let mut evil = Contract::new("hive.evil.v1");
    let p1 = evil.declare(
        "steal",
        AuthPolicy::OpenParticipation,
        Effect::StateWrite,
        &[],
        "db_id",
    );
    println!("\n== P1 open-participation write is unrepresentable ==");
    match p1 {
        Err(e) => println!("PASS refused: {e}"),
        Ok(()) => {
            println!("FAIL contract admitted an open-participation state write");
            std::process::exit(1);
        }
    }

    // ---- P2..P5: the authorization seam. Refusals must be recorded, not dropped. ----
    let engine = Arc::new(Engine::new(DEFAULT_SHARDS));
    engine.register(contract.clone()).await.expect("register contract");
    let probe_target = dbs[0].id.clone();
    let probe_project = dbs[0].project.clone();
    let probe_team = if dbs[0].team.is_empty() {
        "acme-tenant".to_string()
    } else {
        dbs[0].team.clone()
    };
    println!("\n== P2..P5 authorization seam (target db {}) ==", dbs[0].id);
    let cases: Vec<(&str, Caller, &str, BTreeMap<String, String>)> = vec![
        (
            "untagged tenant",
            Caller {
                tenant: UNTAGGED_TENANT.to_string(),
                actor: "anon".into(),
                owns_target: true,
            },
            "db.write",
            payload(&[
                ("db_id", &probe_target),
                ("project", &probe_project),
                ("bytes", "1"),
            ]),
        ),
        (
            "tenant does not own target",
            Caller {
                tenant: probe_team.clone(),
                actor: "other".into(),
                owns_target: false,
            },
            "db.write",
            payload(&[
                ("db_id", &probe_target),
                ("project", &probe_project),
                ("bytes", "1"),
            ]),
        ),
        (
            "undeclared op kind",
            owner(&probe_team),
            "db.drop_everything",
            payload(&[("db_id", &probe_target)]),
        ),
        (
            "payload schema mismatch",
            owner(&probe_team),
            "db.write",
            payload(&[("db_id", &probe_target), ("surprise", "x")]),
        ),
    ];
    for (name, caller, kind, pl) in cases {
        let r = engine
            .submit(&contract, caller, kind, pl, &probe_target, 1_700_000_000_000, Ok(()))
            .await
            .expect("submit");
        match (r.committed, r.error) {
            (false, Some(e)) => println!("PASS refused [{name}]: {e}"),
            (true, _) => {
                println!("FAIL [{name}] was committed; a gate is missing");
                std::process::exit(1);
            }
            (false, None) => {
                println!("FAIL [{name}] refused with no reason recorded");
                std::process::exit(1);
            }
        }
    }

    // ---- P6: open participation IS permitted -- for a log-only op, and it must not move state. ----
    println!("\n== P6 open participation is admitted, but cannot move state ==");
    let before = engine.index_hash(&contract.id).await.unwrap_or_default();
    let anon = Caller {
        tenant: UNTAGGED_TENANT.to_string(),
        actor: "untrusted-node".into(),
        owns_target: false,
    };
    let r = engine
        .submit(
            &contract,
            anon,
            "db.propose",
            payload(&[("db_id", &probe_target), ("note", "relayed by a stranger")]),
            &probe_target,
            1_700_000_000_000,
            Ok(()),
        )
        .await
        .expect("submit");
    let after = engine.index_hash(&contract.id).await.unwrap_or_default();
    if r.committed && before == after {
        println!("PASS committed (seq {}) and index unchanged: {}", r.seq, after);
    } else {
        println!("FAIL committed={} before={before} after={after}", r.committed);
        std::process::exit(1);
    }

    // ---- P7: real throughput over real records, one task per (round, database). ----
    println!("\n== P7 execution over real database records ==");
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for round in 0..rounds {
        for d in &dbs {
            let c = contract.clone();
            let e = engine.clone();
            let team = if d.team.is_empty() {
                UNTAGGED_TENANT.to_string()
            } else {
                d.team.clone()
            };
            let id = d.id.clone();
            let proj = d.project.clone();
            handles.push(tokio::spawn(async move {
                let caller = if team == UNTAGGED_TENANT {
                    // An untagged record cannot authorize a state write; it is admitted only as the
                    // log-only proposal, so the run still covers it.
                    Caller {
                        tenant: team.clone(),
                        actor: "untagged".into(),
                        owns_target: false,
                    }
                } else {
                    owner(&team)
                };
                let (kind, pl) = if team == UNTAGGED_TENANT {
                    (
                        "db.propose".to_string(),
                        payload(&[("db_id", &id), ("note", "untagged proposal")]),
                    )
                } else {
                    (
                        "db.write".to_string(),
                        payload(&[
                            ("db_id", &id),
                            ("project", &proj),
                            ("bytes", &round.to_string()),
                        ]),
                    )
                };
                e.submit(&c, caller, &kind, pl, &id, 1_700_000_000_000, Ok(())).await
            }));
        }
    }
    let mut committed = 0usize;
    let mut refused = 0usize;
    for h in handles {
        let r = h.await.expect("join").expect("submit");
        if r.committed {
            committed += 1;
        } else {
            refused += 1;
        }
    }
    let elapsed = t0.elapsed();
    let total = committed + refused;
    println!(
        "committed={committed} refused={refused} total={total} in {:?} -> {:.0} ops/s",
        elapsed,
        total as f64 / elapsed.as_secs_f64().max(1e-9)
    );

    // ---- P8: determinism. Three independent recomputations of the same history must agree. ----
    let log: OpLog = engine
        .snapshot_log(&contract.id)
        .await
        .expect("log present");
    let live_hash = engine.index_hash(&contract.id).await.unwrap_or_default();
    println!("\n== P8 determinism ==");
    println!("log entries      : {}", log.len());
    println!("log head         : {}", log.head());
    println!("live index hash  : {live_hash}");

    let c2 = contract.clone();
    let l2 = log.clone();
    let replayed = std::thread::spawn(move || audit::replay(&c2, &l2).map(|i| index_hash(&i)))
        .join()
        .expect("replay thread");
    match replayed {
        Ok(h) if h == live_hash => println!("PASS replay in a second OS thread agrees: {h}"),
        Ok(h) => {
            println!("FAIL replay diverged: live={live_hash} replayed={h}");
            std::process::exit(1);
        }
        Err(b) => {
            println!("FAIL replay reported a breach on honest history: {b}");
            std::process::exit(1);
        }
    }
    let again = audit::replay(&contract, &log).map(|i| index_hash(&i));
    match again {
        Ok(h) if h == live_hash => println!("PASS second in-process replay agrees"),
        Ok(h) => {
            println!("FAIL second replay diverged: {h}");
            std::process::exit(1);
        }
        Err(b) => {
            println!("FAIL second replay breached: {b}");
            std::process::exit(1);
        }
    }

    // ---- P9: tamper detection. Rewrite one committed payload; replay must name the seq. ----
    println!("\n== P9 tamper detection ==");
    let forged_seq = log
        .entries()
        .iter()
        .position(|e| e.ack.success && e.op.kind == "db.write")
        .expect("a committed db.write exists");
    {
        // Rewrite one committed payload in place -- what a dishonest executor would do -- and keep
        // the original acks, so the only way to catch it is to recompute the state.
        let mut op = log.entries()[forged_seq].op.clone();
        op.payload.insert("bytes".to_string(), "999999999".to_string());
        let mut rebuilt = OpLog::new();
        for e in log.entries().iter().take(forged_seq) {
            rebuilt.append(e.op.clone(), e.ack.clone());
        }
        rebuilt.append(op, log.entries()[forged_seq].ack.clone());
        for e in log.entries().iter().skip(forged_seq + 1) {
            rebuilt.append(e.op.clone(), e.ack.clone());
        }
        match audit::replay(&contract, &rebuilt) {
            Err(b) => println!("PASS replay detected the forgery: {b}"),
            Ok(i) => {
                println!("FAIL replay accepted a forged log: {}", index_hash(&i));
                std::process::exit(1);
            }
        }
    }

    // ---- P10: concurrency scaling. Where does contention actually live? ----
    println!("\n== P10 concurrency scaling (same work, different shard counts) ==");
    let work = dbs.len() * rounds;
    for shards in [1usize, 4, 16, 64] {
        let e = Arc::new(Engine::new(shards));
        e.register(contract.clone()).await.expect("register contract");
        let t = Instant::now();
        let mut hs = Vec::new();
        for round in 0..rounds {
            for d in &dbs {
                let c = contract.clone();
                let e = e.clone();
                let team = if d.team.is_empty() {
                    UNTAGGED_TENANT.to_string()
                } else {
                    d.team.clone()
                };
                let id = d.id.clone();
                let proj = d.project.clone();
                hs.push(tokio::spawn(async move {
                    let caller = if team == UNTAGGED_TENANT {
                        Caller {
                            tenant: team.clone(),
                            actor: "untagged".into(),
                            owns_target: false,
                        }
                    } else {
                        owner(&team)
                    };
                    let (kind, pl) = if team == UNTAGGED_TENANT {
                        (
                            "db.propose".to_string(),
                            payload(&[("db_id", &id), ("note", "proposal")]),
                        )
                    } else {
                        (
                            "db.write".to_string(),
                            payload(&[
                                ("db_id", &id),
                                ("project", &proj),
                                ("bytes", &round.to_string()),
                            ]),
                        )
                    };
                    e.submit(&c, caller, &kind, pl, &id, 1_700_000_000_000, Ok(())).await
                }));
            }
        }
        for h in hs {
            let _ = h.await;
        }
        let el = t.elapsed();
        println!(
            "shards={shards:<3} work={work:<6} {:?} -> {:.0} ops/s",
            el,
            work as f64 / el.as_secs_f64().max(1e-9)
        );
    }
    println!("\nNOTE: every op above targets ONE contract, so this is the single-writer floor, not\n\
the scaling curve -- a contract is one writer by design. P11 is the scaling test.");

    // ---- P11: concurrency scaling across CONTRACTS. One contract per real database, so ops on
    // different databases are logically independent and sharding should show up as throughput. ----
    println!("\n== P11 concurrency scaling (one contract per real database) ==");
    let scoped: Vec<Contract> = dbs
        .iter()
        .map(|d| {
            let mut c = contract.clone();
            c.id = format!("{}:{}", contract.id, d.id);
            c
        })
        .collect();
    println!("contracts       : {}", scoped.len());
    let work = dbs.len() * rounds;
    let mut baseline = 0.0f64;
    for shards in [1usize, 4, 16, 64] {
        let e = Arc::new(Engine::new(shards));
        for c in &scoped {
            e.register(c.clone()).await.expect("register contract");
        }
        let t = Instant::now();
        let mut hs = Vec::new();
        for round in 0..rounds {
            for (i, d) in dbs.iter().enumerate() {
                let c = scoped[i].clone();
                let e = e.clone();
                let team = if d.team.is_empty() {
                    UNTAGGED_TENANT.to_string()
                } else {
                    d.team.clone()
                };
                let id = d.id.clone();
                let proj = d.project.clone();
                hs.push(tokio::spawn(async move {
                    let caller = if team == UNTAGGED_TENANT {
                        Caller {
                            tenant: team.clone(),
                            actor: "untagged".into(),
                            owns_target: false,
                        }
                    } else {
                        owner(&team)
                    };
                    let (kind, pl) = if team == UNTAGGED_TENANT {
                        (
                            "db.propose".to_string(),
                            payload(&[("db_id", &id), ("note", "proposal")]),
                        )
                    } else {
                        (
                            "db.write".to_string(),
                            payload(&[
                                ("db_id", &id),
                                ("project", &proj),
                                ("bytes", &round.to_string()),
                            ]),
                        )
                    };
                    e.submit(&c, caller, &kind, pl, &id, 1_700_000_000_000, Ok(()))
                        .await
                        .expect("submit")
                }));
            }
        }
        let mut ok = 0usize;
        for h in hs {
            if h.await.expect("join").committed {
                ok += 1;
            }
        }
        let el = t.elapsed();
        let rate = work as f64 / el.as_secs_f64().max(1e-9);
        if shards == 1 {
            baseline = rate;
        }
        println!(
            "shards={shards:<3} committed={ok:<5} work={work:<6} {:?} -> {:.0} ops/s ({:.2}x vs 1 shard)",
            el,
            rate,
            rate / baseline.max(1e-9)
        );
    }

    // ---- P12..P17: the six defects a live adversarial audit demonstrated, re-run here ----
    // Each probe below is the attack that exposed its defect, executed against the real contract,
    // the real executor and the real replay auditor. They live in the harness so the attack is
    // repeatable, not so it is skipped: the crate ships no test files by policy.
    let mut defects = 0usize;

    // ---- P12: the backend is DOWN. An ack must record EXECUTION, not admission. ----
    println!("\n== P12 the backend is down: an ack records execution, not admission ==");
    let down_engine = Engine::new(DEFAULT_SHARDS);
    down_engine.register(contract.clone()).await.expect("register contract");
    let down_db = "db-down-0001";
    let mut reported_committed = 0usize;
    for i in 0..50u64 {
        let r = down_engine
            .submit(
                &contract,
                owner(&probe_team),
                "db.write",
                payload(&[
                    ("bytes", "4096"),
                    ("db_id", down_db),
                    ("project", &probe_project),
                ]),
                down_db,
                1_700_000_000_000 + i,
                Err("backend: connection refused".to_string()),
            )
            .await
            .expect("submit");
        if r.committed {
            reported_committed += 1;
        }
    }
    let down_log = down_engine.snapshot_log(&contract.id).await.expect("log");
    let false_acks = down_log.entries().iter().filter(|e| !e.ack.success).count();
    let true_acks = down_log.entries().iter().filter(|e| e.ack.success).count();
    let down_index = audit::replay(&contract, &down_log).expect("replay of failed writes");
    println!(
        "  entries={} success=false:{} success=true:{} reported_committed={}",
        down_log.len(),
        false_acks,
        true_acks,
        reported_committed
    );
    println!(
        "  index writes key for the down database: {:?}",
        down_index.get(&format!("{probe_team}/{down_db}/writes"))
    );
    check(
        &mut defects,
        false_acks == 50 && true_acks == 0 && reported_committed == 0,
        "a write the backend failed is recorded as success=false, not true",
        format!("{false_acks} acks carry success=false, {true_acks} carry success=true, \
                 {reported_committed} reported committed (was: 50 committed, 0 success=false)"),
    );
    check(
        &mut defects,
        !down_index.keys().any(|k| k.ends_with("/writes")),
        "a failed write applies no mutation",
        format!("{} index keys, none of them a write counter", down_index.len()),
    );

    // The same op with a working backend still commits: the fix must not turn success into failure.
    let up = down_engine
        .submit(
            &contract,
            owner(&probe_team),
            "db.write",
            payload(&[
                ("bytes", "4096"),
                ("db_id", down_db),
                ("project", &probe_project),
            ]),
            down_db,
            1_700_000_000_100,
            Ok(()),
        )
        .await
        .expect("submit");
    check(
        &mut defects,
        up.committed && up.error.is_none(),
        "an executed write is still recorded as success=true",
        format!("committed={} error={:?}", up.committed, up.error),
    );

    // ---- P13: an undeclared kind reaching the executor is an error, never a panic. ----
    println!("\n== P13 an undeclared kind is refused by the executor, not a panic ==");
    let mut ex13 = Executor::new(contract.clone());
    let r13 = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ex13.commit(
            Op {
                seq: 0,
                contract: contract.id.clone(),
                tenant: probe_team.clone(),
                actor: "p13".into(),
                target: probe_target.clone(),
                kind: "db.grant_superuser".into(),
                payload: payload(&[("db_id", &probe_target), ("project", &probe_project)]),
            },
            1_700_000_000_000,
            Ok(()),
        )
    }));
    match &r13 {
        Ok(Err(e)) => println!("  commit returned: {e}"),
        Ok(Ok(_)) => println!("  commit COMMITTED an undeclared kind"),
        Err(_) => println!("  commit PANICKED"),
    }
    let refused_undeclared = match &r13 {
        Ok(Err(hive_contract::exec::ExecError::UndeclaredKind { kind })) => {
            kind == "db.grant_superuser"
        }
        _ => false,
    };
    check(
        &mut defects,
        refused_undeclared && ex13.log.is_empty(),
        "commit on an undeclared kind returns Err and leaves no partial entry",
        format!("returned={} log_len={}", r13.is_ok(), ex13.log.len()),
    );

    // ---- P14: an effect-violating op is recorded as refused, not dropped. ----
    println!("\n== P14 an op the executor refuses still lands in the log ==");
    let mut c14 = Contract::new("hive.misdeclared.v1");
    c14.declare(
        "db.write",
        AuthPolicy::TenantAuthorized,
        Effect::LogOnly,
        &["bytes", "db_id", "project"],
        "db_id",
    )
    .expect("db.write declared log-only");
    let e14 = Engine::new(DEFAULT_SHARDS);
    let r14 = e14
        .submit(
            &c14,
            owner("acme"),
            "db.write",
            payload(&[("bytes", "64"), ("db_id", "db-14"), ("project", "p")]),
            "db-14",
            1_700_000_000_000,
            Ok(()),
        )
        .await
        .expect("submit");
    let l14 = e14.snapshot_log(&c14.id).await.expect("log");
    println!(
        "  committed={} error={:?} log_len={}",
        r14.committed,
        r14.error,
        l14.len()
    );
    check(
        &mut defects,
        !r14.committed
            && l14.len() == 1
            && !l14.entries()[0].ack.success
            && l14.entries()[0]
                .ack
                .error
                .as_deref()
                .unwrap_or("")
                .contains("declared log-only"),
        "the refused op is in the log with success=false and its reason",
        format!(
            "log_len={} (was 0) success={} error={:?}",
            l14.len(),
            l14.entries().first().map(|e| e.ack.success).unwrap_or(true),
            l14.entries().first().and_then(|e| e.ack.error.clone())
        ),
    );

    // ---- P15: the target must be the database the validated payload names. ----
    println!("\n== P15 a target that is not the payload's db_id is refused ==");
    let e15 = Engine::new(DEFAULT_SHARDS);
    let r15 = e15
        .submit(
            &contract,
            owner("acme"),
            "db.create",
            payload(&[("db_id", "db-REAL"), ("kind", "sqlite"), ("project", "p")]),
            "db-OTHER",
            1_700_000_000_000,
            Ok(()),
        )
        .await
        .expect("submit");
    let idx15 = e15.audit(&contract).await.expect("replay");
    println!("  committed={} error={:?}", r15.committed, r15.error);
    check(
        &mut defects,
        !r15.committed
            && r15.error.as_deref()
                == Some(
                    OpError::TargetMismatch {
                        target: "db-OTHER".to_string(),
                        expected: "db-REAL".to_string(),
                    }
                    .to_string()
                )
                .as_deref()
            && !idx15.keys().any(|k| k.contains("db-OTHER")),
        "a mismatched target is refused and writes nothing",
        format!(
            "error={:?}; index keys={:?} (was committed=true, key acme/db-OTHER/status)",
            r15.error,
            idx15.keys().collect::<Vec<_>>()
        ),
    );

    // ---- P16: no two (tenant, target) pairs may address one key. ----
    println!("\n== P16 colliding tenant/target pairs do not alias to one key ==");
    let mut ex16 = Executor::new(contract.clone());
    let collide: [(&str, &str); 2] = [("alice", "b/c"), ("alice/b", "c")];
    for (t, g) in collide {
        ex16.commit(
            Op {
                seq: 0,
                contract: contract.id.clone(),
                tenant: t.to_string(),
                actor: "p16".into(),
                target: g.to_string(),
                kind: "db.create".into(),
                payload: payload(&[("db_id", g), ("kind", "sqlite"), ("project", "p")]),
            },
            1_700_000_000_000,
            Ok(()),
        )
        .expect("commit");
    }
    let keys16: Vec<&String> = ex16.index.keys().collect();
    println!("  keys: {keys16:?}");
    check(
        &mut defects,
        ex16.index.len() == 2,
        "two distinct (tenant, target) pairs produce two keys, not one",
        format!(
            "{} keys for 2 pairs (was 1: both pairs built alice/b/c)",
            ex16.index.len()
        ),
    );
    // And one delete reaches only its own namespace.
    ex16.commit(
        Op {
            seq: 0,
            contract: contract.id.clone(),
            tenant: "alice".into(),
            actor: "p16".into(),
            target: "b/c".into(),
            kind: "db.delete".into(),
            payload: payload(&[("db_id", "b/c"), ("project", "p")]),
        },
        1_700_000_000_001,
        Ok(()),
    )
    .expect("commit");
    check(
        &mut defects,
        ex16.index.len() == 1 && ex16.index.keys().all(|k| k.starts_with("alice%2Fb/")),
        "a delete on one pair does not reach the other's keys",
        format!("{:?} left", ex16.index.keys().collect::<Vec<_>>()),
    );

    // ---- P17: the properties that already held still hold. ----
    println!("\n== P17 preserved properties ==");
    let e17 = Engine::new(DEFAULT_SHARDS);
    e17.register(contract.clone()).await.expect("register contract");
    for i in 0..500u64 {
        let t = format!("db-{}", i % 5);
        e17.submit(
            &contract,
            owner("acme"),
            "db.write",
            payload(&[("bytes", "512"), ("db_id", &t), ("project", "p")]),
            &t,
            1_700_000_000_000 + i,
            Ok(()),
        )
        .await
        .expect("submit");
    }
    let l17 = e17.snapshot_log(&contract.id).await.expect("log");
    let live17 = e17.index_hash(&contract.id).await.expect("hash");
    let c17 = contract.clone();
    let l17a = l17.clone();
    let t1 = std::thread::spawn(move || audit::replay(&c17, &l17a).map(|i| index_hash(&i)));
    let h1 = t1.join().expect("replay thread").expect("replay clean");
    let h2 = audit::replay(&contract, &l17).map(|i| index_hash(&i)).expect("replay clean");
    let h3 = audit::replay(&contract, &l17).map(|i| index_hash(&i)).expect("replay clean");
    println!("  500 ops: live={:.16} replay1={:.16} replay2={:.16} replay3={:.16}", live17, h1, h2, h3);
    check(
        &mut defects,
        h1 == live17 && h2 == live17 && h3 == live17,
        "a 500-op log replayed three times gives one index hash",
        format!("{} == {} == {} == {}", &live17[..16], &h1[..16], &h2[..16], &h3[..16]),
    );
    let mut forward = Index::new();
    let mut backward = Index::new();
    for i in 0..10u64 {
        forward.insert(format!("acme/db-{i}/status"), "live".into());
    }
    for i in (0..10u64).rev() {
        backward.insert(format!("acme/db-{i}/status"), "live".into());
    }
    check(
        &mut defects,
        index_hash(&forward) == index_hash(&backward),
        "the index hash is insertion-order independent",
        format!("{} == {}", &index_hash(&forward)[..16], &index_hash(&backward)[..16]),
    );
    let undeclared = e17
        .submit(
            &contract,
            owner("acme"),
            "db.drop_everything",
            payload(&[("db_id", "db-0")]),
            "db-0",
            1_700_000_000_000,
            Ok(()),
        )
        .await
        .expect("submit");
    check(
        &mut defects,
        !undeclared.committed
            && undeclared.error.as_deref() == Some(&OpError::UndeclaredKind("db.drop_everything".to_string()).to_string()),
        "submit still refuses an undeclared kind",
        format!("{:?}", undeclared.error),
    );

    if defects == 0 {
        println!("\nALL PROBES PASSED");
    } else {
        println!("\n{defects} PROBE(S) FAILED");
        std::process::exit(1);
    }
}
