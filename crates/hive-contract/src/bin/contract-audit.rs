//! Live witness harness for the contract/execution layer.
//!
//! Not a test file and not a mock: it runs the real contract, the real executor and the real replay
//! auditor against a REAL platform snapshot (`$HIVE_DATA/state.json` copied off a fleet node), and
//! prints what happened. Every claim the design makes has a probe here that could falsify it.
//!
//! Usage: `contract-audit --state <state.json> [--rounds N]`
//!
//! Connection credentials in the snapshot are never read and never printed.

use hive_contract::contract::{
    managed_db_contract, AuthPolicy, Caller, Contract, Effect, UNTAGGED_TENANT,
};
use hive_contract::engine::{Engine, DEFAULT_SHARDS};
use hive_contract::oplog::OpLog;
use hive_contract::{audit, index_hash};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

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
    let p1 = evil.declare("steal", AuthPolicy::OpenParticipation, Effect::StateWrite, &[]);
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
    engine.register(contract.clone()).await;
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
            .submit(&contract, caller, kind, pl, &probe_target, 1_700_000_000_000)
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
                e.submit(&c, caller, &kind, pl, &id, 1_700_000_000_000).await
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
        e.register(contract.clone()).await;
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
                    e.submit(&c, caller, &kind, pl, &id, 1_700_000_000_000).await
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
            e.register(c.clone()).await;
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
                    e.submit(&c, caller, &kind, pl, &id, 1_700_000_000_000)
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

    println!("\nALL PROBES PASSED");
}
