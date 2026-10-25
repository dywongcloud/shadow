//! Live witness harness for contract replication.
//!
//! Not a test file and not a mock: it drives the real `Engine`/`Replica` code over the REAL platform
//! snapshot (`$HIVE_DATA/state.json` copied off fc-sanjose), then BREAKS each replication invariant
//! on purpose -- a stale offer, a fork, a fabricated entry, a forged ack, a gap, a wholesale replace
//! from a backup owner -- and shows the merge refusing each one. Convergence is proved by landing
//! three replicas, fed the same history at different heights and in different orders, on one index
//! hash, and by reconciling that index against canonical state.
//!
//! Usage: `contract-replicate --state <state.json>`
//!
//! `connection` (passwords, connection strings) is never read and never printed.

use hive_contract::contract::{record_tenant, Caller, Contract, UNTAGGED_TENANT};
use hive_contract::contracts::managed_database::managed_db_contract;
use hive_contract::engine::Engine;
use hive_contract::exec::index_hash;
use hive_contract::oplog::OpLog;
use hive_contract::reconcile::{self, CanonicalDb, DivergenceKind};
use hive_contract::replicate::{AdoptSource, MergeError, Replica, ReplicaSnapshot};
use std::collections::BTreeMap;

#[derive(serde::Deserialize)]
struct RealDb {
    #[serde(default)]
    id: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    team: String,
    #[serde(default)]
    status: String,
}

#[derive(serde::Deserialize)]
struct Snapshot {
    #[serde(default)]
    databases: Vec<RealDb>,
    #[serde(default)]
    database_tombstones: BTreeMap<String, u64>,
}

/// One operation to execute, captured so the SAME plan can be replayed against a second writer in a
/// different order to produce a genuine fork.
#[derive(Clone)]
struct PlanOp {
    kind: &'static str,
    tenant: String,
    actor: &'static str,
    owns: bool,
    payload: BTreeMap<String, String>,
    target: String,
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

fn probe(failures: &mut usize, ok: bool, label: &str, detail: String) {
    if ok {
        println!("PASS {label}: {detail}");
    } else {
        println!("FAIL {label}: {detail}");
        *failures += 1;
    }
}

/// Execute a plan and return, per op, whether it was committed.
async fn run_plan(
    engine: &Engine,
    contract: &Contract,
    plan: &[PlanOp],
    start_ts: u64,
) -> Vec<bool> {
    let mut out = Vec::with_capacity(plan.len());
    let mut ts = start_ts;
    for p in plan {
        let r = engine
            .submit(
                contract,
                Caller {
                    tenant: p.tenant.clone(),
                    actor: p.actor.to_string(),
                    owns_target: p.owns,
                },
                p.kind,
                p.payload.clone(),
                &p.target,
                ts,
            )
            .await;
        ts += 1;
        out.push(matches!(r, Ok(s) if s.committed));
    }
    out
}

// ---- log surgery -------------------------------------------------------------------------------
// A peer's log arrives over the wire as entries, so the harness builds peer and forged logs the same
// way replication would: through serde. Nothing here reaches into `OpLog`'s fields, and nothing here
// can produce a log the merge's own re-derivation would accept.

fn entries_json(log: &OpLog) -> Vec<serde_json::Value> {
    let v = serde_json::to_value(log).expect("log serializes");
    match v {
        serde_json::Value::Object(mut m) => match m.remove("entries") {
            Some(serde_json::Value::Array(a)) => a,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn head_of(entries: &[serde_json::Value]) -> String {
    entries
        .last()
        .and_then(|e| e["entry_hash"].as_str())
        .unwrap_or("")
        .to_string()
}

fn log_from(entries: Vec<serde_json::Value>, head: &str) -> OpLog {
    serde_json::from_value(serde_json::json!({ "entries": entries, "head": head }))
        .expect("log shape")
}

/// A peer that has synced only the first `n` entries: a real height, not a forgery.
fn at_height(entries: &[serde_json::Value], n: usize) -> OpLog {
    let e: Vec<serde_json::Value> = entries.iter().take(n).cloned().collect();
    let h = head_of(&e);
    log_from(e, &h)
}

/// Same ops, but the publisher claims a different index at `idx`. The entry hash does NOT cover the
/// ack, so the chain still verifies -- only re-deriving the index catches this.
fn forged_ack(entries: &[serde_json::Value], idx: usize) -> OpLog {
    let mut e = entries.to_vec();
    e[idx]["ack"]["index_hash"] = serde_json::json!("0".repeat(64));
    let h = head_of(entries);
    log_from(e, &h)
}

/// One op's payload rewritten under the original head: a fabricated entry, caught by re-derivation.
fn forged_entry(entries: &[serde_json::Value], idx: usize) -> OpLog {
    let mut e = entries.to_vec();
    e[idx]["op"]["payload"]["project"] = serde_json::json!("forged-project");
    let h = head_of(entries);
    log_from(e, &h)
}

/// A hole: entry `idx` removed, so every later entry arrives one seq too high.
fn with_gap(entries: &[serde_json::Value], idx: usize) -> OpLog {
    let mut e = entries.to_vec();
    e.remove(idx);
    let h = head_of(&e);
    log_from(e, &h)
}

fn err_label(e: &MergeError) -> &'static str {
    match e {
        MergeError::Regress { .. } => "Regress",
        MergeError::Fork { .. } => "Fork",
        MergeError::Append(_) => "Append",
        MergeError::DivergentAck { .. } => "DivergentAck",
        MergeError::UndeclaredOp { .. } => "UndeclaredOp",
        MergeError::EffectViolation { .. } => "EffectViolation",
        MergeError::BrokenChain(_) => "BrokenChain",
        MergeError::NotAuthorizedWholesale { .. } => "NotAuthorizedWholesale",
        MergeError::ContractMismatch { .. } => "ContractMismatch",
        MergeError::Empty => "Empty",
        MergeError::Unparsable => "Unparsable",
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

    // ---- P1: the real surface ------------------------------------------------------------------
    let mut dbs: Vec<CanonicalDb> = snap
        .databases
        .iter()
        .filter(|d| !snap.database_tombstones.contains_key(&d.id))
        .map(|d| CanonicalDb {
            id: d.id.clone(),
            team: d.team.clone(),
            project: d.project.clone(),
            status: d.status.clone(),
        })
        .collect();
    dbs.sort_by(|a, b| a.id.cmp(&b.id));
    let canonical = reconcile::project(&dbs, &snap.database_tombstones);
    println!("\n== P1 real snapshot ==");
    println!(
        "databases        : {} ({} tombstoned)",
        snap.databases.len(),
        snap.database_tombstones.len()
    );
    println!("live rows        : {}", dbs.len());
    println!("projected keys   : {}", canonical.len());
    probe(
        &mut failures,
        !dbs.is_empty(),
        "real input loaded",
        format!("{} live databases", dbs.len()),
    );
    if dbs.is_empty() {
        println!("FAIL real snapshot has no live databases to replicate");
        std::process::exit(1);
    }

    // ---- P2: the owner executes the real mutation set -------------------------------------------
    let contract: Contract = managed_db_contract();
    let engine = Engine::new(hive_contract::engine::DEFAULT_SHARDS);
    engine.register(contract.clone()).await;

    let mut plan: Vec<PlanOp> = Vec::new();
    for d in &dbs {
        plan.push(PlanOp {
            kind: "db.create",
            tenant: record_tenant(Some(&d.team)),
            actor: "owner:fc-sanjose",
            owns: true,
            payload: payload(&[
                ("db_id", &d.id),
                ("kind", "sqlite"),
                ("project", &d.project),
            ]),
            target: d.id.clone(),
        });
    }
    let write_count = dbs.len().min(4) * 2;
    for d in dbs.iter().take(4) {
        for n in 0..2 {
            plan.push(PlanOp {
                kind: "db.write",
                tenant: record_tenant(Some(&d.team)),
                actor: "owner:fc-sanjose",
                owns: true,
                payload: payload(&[
                    ("db_id", &d.id),
                    ("project", &d.project),
                    ("bytes", &format!("{}", (n + 1) * 4096)),
                ]),
                target: d.id.clone(),
            });
        }
    }
    // Two refusals the platform's own gate produced: recorded, not silently dropped.
    for d in dbs.iter().take(2) {
        plan.push(PlanOp {
            kind: "db.delete",
            tenant: record_tenant(Some(&d.team)),
            actor: "not-owner",
            owns: false,
            payload: payload(&[("db_id", &d.id), ("project", &d.project)]),
            target: d.id.clone(),
        });
    }
    // Permissionless participation: an unauthenticated proposer, no allowlisting.
    plan.push(PlanOp {
        kind: "db.propose",
        tenant: UNTAGGED_TENANT.to_string(),
        actor: "unknown-node:fc-sanjose-7",
        owns: false,
        payload: payload(&[
            ("db_id", &dbs[0].id),
            ("note", "relayed-without-allowlist"),
        ]),
        target: dbs[0].id.clone(),
    });

    let results = run_plan(&engine, &contract, &plan, 1_790_000_000_000u64).await;
    let committed = results.iter().filter(|r| **r).count();
    let refused = results.len() - committed;
    let owner_log = engine
        .snapshot_log(&contract.id)
        .await
        .expect("owner log exists");
    let owner_index = engine.audit(&contract).await.expect("owner log replays clean");
    let owner_hash = index_hash(&owner_index);
    println!("\n== P2 owner execution ==");
    println!("committed ops    : {committed} ({} db.create + {write_count} db.write + 1 propose)", dbs.len());
    println!("refused ops      : {refused}");
    println!("log entries      : {}", owner_log.len());
    println!("owner index hash : {owner_hash}");
    probe(
        &mut failures,
        owner_log.len() == plan.len(),
        "every op is in the log, admitted or refused",
        format!("{} entries for {} ops", owner_log.len(), plan.len()),
    );
    let n = results.len();
    probe(
        &mut failures,
        !results[n - 3] && !results[n - 2],
        "a caller that does not own the target is still refused",
        "2 non-owner db.delete ops recorded as refusals".to_string(),
    );
    let propose_idx = n - 1;
    let propose_moved_nothing = propose_idx > 0
        && owner_log.entries()[propose_idx].ack.index_hash
            == owner_log.entries()[propose_idx - 1].ack.index_hash;
    probe(
        &mut failures,
        results[propose_idx] && propose_moved_nothing,
        "an unlisted node's proposal is admitted and moves no state",
        format!(
            "committed seq {} and index unchanged at {:.8}",
            owner_log.entries()[propose_idx].ack.seq,
            owner_log.entries()[propose_idx].ack.index_hash
        ),
    );

    let report = reconcile::reconcile(&canonical, &owner_index);
    println!("compared keys    : {}", report.compared);
    println!("contract-only    : {}", report.contract_only.len());
    probe(
        &mut failures,
        report.in_sync(),
        "the owner's index still reconciles against canonical state",
        format!(
            "{} divergences over {} attestable keys ({} contract-local)",
            report.divergences.len(),
            report.compared,
            report.contract_only.len()
        ),
    );
    if !report.in_sync() {
        for d in report.divergences.iter().take(5) {
            println!(
                "   divergence {}: canonical={:?} contract={:?}",
                d.kind.label(),
                d.canonical,
                d.contract
            );
        }
    }

    // ---- P3: three replicas converge from different heights, in different orders ----------------
    println!("\n== P3 convergence ==");
    let entries = entries_json(&owner_log);
    let heights: [usize; 3] = [
        (owner_log.len() / 3).max(1),
        (owner_log.len() * 2 / 3).max(1),
        owner_log.len(),
    ];
    println!("peer heights     : {heights:?} of {}", owner_log.len());

    // A: straight to the head.
    let mut a = Replica::new(contract.clone());
    let ra = a.merge(&owner_log).expect("A merges the head");
    let ha = a.index_hash();
    println!("A appended       : {} -> {:.8}", ra.appended, ha);

    // B: the nested order -- each offer strictly taller than the last.
    let mut b = Replica::new(contract.clone());
    for h in heights {
        b.merge(&at_height(&entries, h)).expect("B merges");
    }
    let hb = b.index_hash();

    // C: a TALLER offer first, then a shorter, stale one. The stale offer must not roll anything back.
    let mut c = Replica::new(contract.clone());
    c.merge(&at_height(&entries, heights[1])).expect("C first");
    let mid = c.log.len();
    let stale = c.merge(&at_height(&entries, heights[0])).expect("C stale");
    c.merge(&at_height(&entries, heights[2])).expect("C head");
    let hc = c.index_hash();
    println!(
        "C stale offer    : skipped_stale={} len {} -> {} -> {}",
        stale.skipped_stale,
        mid,
        stale.len_after,
        c.log.len()
    );
    probe(
        &mut failures,
        stale.skipped_stale && stale.appended == 0 && c.log.len() == owner_log.len(),
        "a stale offer is skipped, never rolled back",
        format!(
            "held {} entries through a {}-entry offer",
            stale.len_after,
            heights[0]
        ),
    );
    probe(
        &mut failures,
        ha == owner_hash && hb == owner_hash && hc == owner_hash,
        "three replicas fed at different heights and in different orders converge",
        format!("A={ha:.8} B={hb:.8} C={hc:.8} owner={owner_hash:.8}"),
    );

    let dup = a.merge(&owner_log).expect("duplicate merge is legal");
    probe(
        &mut failures,
        dup.appended == 0 && dup.duplicates == owner_log.len() && a.index_hash() == ha,
        "a duplicate merge is a no-op",
        format!("{} duplicates, index unchanged", dup.duplicates),
    );

    // ---- P4: break each invariant on purpose ----------------------------------------------------
    println!("\n== P4 refusals ==");
    let mid_idx = owner_log.len() / 2;

    let mut r = Replica::new(contract.clone());
    let e = r
        .merge(&forged_ack(&entries, mid_idx))
        .expect_err("forged ack must be refused");
    probe(
        &mut failures,
        matches!(e, MergeError::DivergentAck { .. }),
        "a forged ack is caught by re-deriving the index",
        format!("{}: {e}", err_label(&e)),
    );

    let mut r = Replica::new(contract.clone());
    let e = r
        .merge(&forged_entry(&entries, mid_idx))
        .expect_err("forged entry must be refused");
    probe(
        &mut failures,
        matches!(e, MergeError::Append(_)),
        "a rewritten payload is caught before it is appended",
        format!("{}: {e}", err_label(&e)),
    );
    probe(
        &mut failures,
        r.log.is_empty(),
        "a rejected batch leaves the replica untouched (no partial merge)",
        "0 entries held".to_string(),
    );

    let mut r = Replica::new(contract.clone());
    let e = r
        .merge(&with_gap(&entries, 1))
        .expect_err("a hole must be refused");
    probe(
        &mut failures,
        matches!(e, MergeError::Append(_)),
        "a hole in the log is refused, never back-filled",
        format!("{}: {e}", err_label(&e)),
    );

    // A real fork: a second honest writer executing the SAME plan in a rotated order.
    let alt_engine = Engine::new(hive_contract::engine::DEFAULT_SHARDS);
    alt_engine.register(contract.clone()).await;
    let mut alt_plan = plan.clone();
    alt_plan.rotate_left(1);
    run_plan(&alt_engine, &contract, &alt_plan, 1_790_000_000_000u64).await;
    let alt_log = alt_engine
        .snapshot_log(&contract.id)
        .await
        .expect("alt log exists");
    let mut held = Replica::new(contract.clone());
    held.merge(&owner_log).expect("held merges owner");
    let e = held.merge(&alt_log).expect_err("a fork must be refused");
    probe(
        &mut failures,
        matches!(e, MergeError::Fork { .. }),
        "two honest writers produce a fork, not a merge",
        format!("{} ({}-entry logs): {e}", err_label(&e), alt_log.len()),
    );
    probe(
        &mut failures,
        held.log.len() == owner_log.len() && held.index_hash() == owner_hash,
        "a forked offer does not move the replica",
        format!("still {} entries, index unchanged", held.log.len()),
    );

    // A replica running an OLDER contract: the same log, replayed against fewer declared ops.
    let mut older = Contract::new(&contract.id);
    older
        .declare(
            "db.create",
            hive_contract::contract::AuthPolicy::TenantAuthorized,
            hive_contract::contract::Effect::StateWrite,
            &["db_id", "kind", "project"],
        )
        .expect("db.create");
    let mut r = Replica::new(older);
    let e = r.merge(&owner_log).expect_err("an undeclared op is a breach");
    probe(
        &mut failures,
        matches!(e, MergeError::UndeclaredOp { .. }) && r.log.is_empty(),
        "a log carrying an op the replica's contract does not declare is refused",
        format!("{}: {e}", err_label(&e)),
    );

    // A shorter, divergent history: the only way a deleted key could come back.
    let mut truncated = entries.clone();
    truncated.truncate(heights[0]);
    truncated[0]["op"]["payload"]["project"] = serde_json::json!("forged-project");
    let shorter_head = head_of(&truncated);
    let shorter = log_from(truncated, &shorter_head);
    let mut r = Replica::new(contract.clone());
    r.merge(&owner_log).expect("r holds the head");
    let e = r.merge(&shorter).expect_err("a regression must be refused");
    probe(
        &mut failures,
        matches!(e, MergeError::Regress { .. }),
        "a shorter divergent history is refused, never rolled back",
        format!("{}: {e}", err_label(&e)),
    );

    // ---- P5: wholesale adoption carries the fleet's own authority predicate ---------------------
    println!("\n== P5 wholesale adoption ==");
    let empty_snap = Replica::new(contract.clone()).snapshot();
    let mut r = Replica::new(contract.clone());
    let e = r
        .adopt_wholesale(
            &empty_snap,
            &AdoptSource {
                owner: "fc-sanjose".to_string(),
                owner_is_chain_head: true,
                from_election: false,
                chain_configured: true,
            },
        )
        .expect_err("an empty snapshot is never adopted");
    probe(
        &mut failures,
        matches!(e, MergeError::Empty),
        "an empty snapshot is never adopted",
        format!("{}: {e}", err_label(&e)),
    );
    let parse_err = ReplicaSnapshot::parse(b"{ not json")
        .err()
        .map(|e| err_label(&e))
        .unwrap_or("none");
    probe(
        &mut failures,
        parse_err == "Unparsable",
        "an unparsable snapshot is never adopted",
        parse_err.to_string(),
    );

    let mut holder = Replica::new(contract.clone());
    holder.merge(&owner_log).expect("holder merges owner");
    let owner_snap = holder.snapshot();
    println!("snapshot digest  : {}", owner_snap.digest());

    let backup = AdoptSource {
        owner: "fc-phoenix".to_string(),
        owner_is_chain_head: false,
        from_election: false,
        chain_configured: true,
    };
    let mut r = Replica::new(contract.clone());
    let e = r
        .adopt_wholesale(&owner_snap, &backup)
        .expect_err("a backup owner may not wholesale-replace");
    probe(
        &mut failures,
        matches!(e, MergeError::NotAuthorizedWholesale { .. }) && r.log.is_empty(),
        "a backup owner may not wholesale-replace; it degrades to merge",
        format!("{}: {e}", err_label(&e)),
    );

    let fallback = AdoptSource {
        owner: "fc-virginia".to_string(),
        owner_is_chain_head: false,
        from_election: false,
        chain_configured: true,
    };
    probe(
        &mut failures,
        !fallback.may_adopt_wholesale(),
        "a Fallback forwarding target is never an authority",
        fallback.reason().to_string(),
    );

    let head = AdoptSource {
        owner: "fc-sanjose".to_string(),
        owner_is_chain_head: true,
        from_election: false,
        chain_configured: true,
    };
    let mut r = Replica::new(contract.clone());
    let rep = r
        .adopt_wholesale(&owner_snap, &head)
        .expect("the chain head may wholesale-replace");
    probe(
        &mut failures,
        r.index_hash() == owner_hash && rep.appended == owner_log.len(),
        "the strict chain head may wholesale-replace, and the index is re-derived",
        format!(
            "adopted {} entries, index {:.8}",
            rep.appended,
            r.index_hash()
        ),
    );

    let election = AdoptSource {
        owner: "fc-sanjose".to_string(),
        owner_is_chain_head: false,
        from_election: true,
        chain_configured: false,
    };
    let mut r = Replica::new(contract.clone());
    let ok = r.adopt_wholesale(&owner_snap, &election).is_ok();
    probe(
        &mut failures,
        ok && r.index_hash() == owner_hash,
        "the identity election on a chain-less mesh is an authority",
        format!("adopted, index {:.8}", r.index_hash()),
    );

    let mut r = Replica::new(contract.clone());
    let e = r
        .adopt_wholesale(
            &owner_snap,
            &AdoptSource {
                owner: "fc-sanjose".to_string(),
                owner_is_chain_head: false,
                from_election: false,
                chain_configured: false,
            },
        )
        .expect_err("no chain and no election is no authority");
    probe(
        &mut failures,
        matches!(e, MergeError::NotAuthorizedWholesale { .. }),
        "with no chain and no election there is no authority to adopt from",
        format!("{}: {e}", err_label(&e)),
    );

    let mut other = Replica::new(Contract::new("hive.other.v1"));
    let e = other
        .adopt_wholesale(&owner_snap, &head)
        .expect_err("a snapshot for another contract");
    probe(
        &mut failures,
        matches!(e, MergeError::ContractMismatch { .. }),
        "a snapshot for another contract is refused",
        format!("{}: {e}", err_label(&e)),
    );

    // ---- P6: the replica's index is the owner's, reconciled against canonical state -------------
    println!("\n== P6 replica vs canonical state ==");
    let mut final_replica = Replica::new(contract.clone());
    final_replica.merge(&owner_log).expect("final merge");
    let final_report = reconcile::reconcile(&canonical, &final_replica.index);
    probe(
        &mut failures,
        final_report.in_sync(),
        "a replica that only ever merged still reconciles against canonical state",
        format!(
            "0 divergences over {} attestable keys ({} contract-local: write counters, last bytes)",
            final_report.compared,
            final_report.contract_only.len()
        ),
    );
    probe(
        &mut failures,
        final_report.count(DivergenceKind::MissingFromContract) == 0
            && final_report.count(DivergenceKind::StaleInContract) == 0,
        "no database is missing from or stale in the replicated index",
        format!(
            "missing={} stale={} mismatched={}",
            final_report.count(DivergenceKind::MissingFromContract),
            final_report.count(DivergenceKind::StaleInContract),
            final_report.count(DivergenceKind::ValueMismatch)
        ),
    );

    println!();
    if failures == 0 {
        println!("ALL PROBES PASSED");
    } else {
        println!("{failures} PROBE(S) FAILED");
        std::process::exit(1);
    }
}
