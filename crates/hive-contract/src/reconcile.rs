//! Canonical-state reconciliation: the check that the contract log has not silently diverged from
//! the platform's ONE canonical state.
//!
//! Why this module exists. The contract layer is additive: a database mutation reaches the platform
//! through `crates/hive-cloud/src/admin.rs` and its gates (`tenant()` at `admin.rs:3284`,
//! `require_project` at `admin.rs:3368`, `write_scope` at `admin.rs:13831`) and only records into the
//! oplog afterwards. Nothing about that ordering is enforced by the type system, so the failure mode
//! is not "the log lies" -- it is "the log is silent": a new mutation path that skips `submit` moves
//! canonical state and the oplog keeps claiming the old truth forever. AGENTS.md names the canonical
//! state rule ("State: replication, routing, relational mirror" -- canonical state is ONE
//! in-memory representation, never derived from or gated on the SQL mirror); this module is the
//! level-triggered check that the contract's index is still a true projection of it.
//!
//! The comparison is a pure diff of two sorted maps. It never mutates either side, never admits a
//! caller and never touches a gate: it is a monitor, in Vitra's sense (whitepaper 2.6 -- "replaying
//! the oplog(s) against the contract's apply function" and comparing to the published index), applied
//! against canonical state instead of against a second replay.
//!
//! COVERAGE BOUNDARY, stated rather than hidden: canonical state can attest only the keys the
//! platform itself stores. For the managed-db contract that is `{tenant}/{db_id}/status`; the write
//! counter and last-bytes keys (`{tenant}/{target}/writes`, `.../last_bytes`) are facts the contract
//! keeps and the platform does not, so they are out of the projection's coverage and are reported as
//! `contract_only` rather than silently ignored.

use crate::exec::Index;
use std::collections::BTreeMap;

/// The platform's view of one managed database: only the fields canonical state can attest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalDb {
    pub id: String,
    /// The STORED team tag, run through [`crate::contract::record_tenant`] before comparison.
    pub team: String,
    pub project: String,
    pub status: String,
}

/// One declared mutation path that belongs in contract scope.
///
/// `wired: false` means the handler has NOT been given the `Engine::submit` call yet, so a mutation
/// through it is invisible to the log -- exactly the divergence [`reconcile`] exists to surface. The
/// count of unwired entries is the honest measure of how much of the surface is actually covered.
#[derive(Clone, Copy, Debug)]
pub struct ScopeEntry {
    pub op_kind: &'static str,
    pub file: &'static str,
    pub line: u32,
    pub wired: bool,
}

/// The mutation surface the managed-db contract claims, located by `path:line`.
///
/// Every entry is a mutation path that already enforces tenancy; the contract layer records the
/// decision, it does not replace it. `wired` is false for all of them today because the wiring lands
/// inside `admin.rs`, which is a shared file -- see the blocker note in the slice report.
pub const DECLARED_SCOPE: &[ScopeEntry] = &[
    ScopeEntry { op_kind: "db.create", file: "crates/hive-cloud/src/admin.rs", line: 13065, wired: false },
    ScopeEntry { op_kind: "db.delete", file: "crates/hive-cloud/src/admin.rs", line: 13528, wired: false },
    // `write_scope` (admin.rs:13831) is the gate all four data-plane writes pass through.
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", line: 13884, wired: false },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", line: 14025, wired: false },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", line: 14136, wired: false },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", line: 14292, wired: false },
];

pub fn scope_report() -> (usize, usize) {
    let wired = DECLARED_SCOPE.iter().filter(|e| e.wired).count();
    (DECLARED_SCOPE.len(), wired)
}

/// The index key canonical state and the contract both speak for one database's liveness.
///
/// Mirrors `exec::apply`'s `db.create` key exactly (`{tenant}/{target}/status`); if the two ever
/// drift, reconciliation reports a divergence on every row instead of silently agreeing.
pub fn status_key(tenant: &str, target: &str) -> String {
    format!("{tenant}/{target}/status")
}

/// Project canonical state into contract-index shape.
///
/// `apply`'s inverse over the attestable keys: a database that is present and not tombstoned has a
/// live status key; a tombstoned one (`DatabaseStore::remove_db`, `databases.rs:740`, tombstone
/// written at :742) has none. Tombstones are permanent there ("absence never erases a row" is the
/// row-merge rule; a tombstone is the erasure), so they are the platform's delete-of-record here.
pub fn project(dbs: &[CanonicalDb], tombstones: &BTreeMap<String, u64>) -> Index {
    let mut out = Index::new();
    for d in dbs {
        let tenant = crate::contract::record_tenant(Some(&d.team));
        if tombstones.contains_key(&d.id) {
            continue;
        }
        let value = match d.status.as_str() {
            "ready" | "provisioning" | "live" => "live".to_string(),
            other => other.to_string(),
        };
        out.insert(status_key(&tenant, &d.id), value);
    }
    out
}

/// Why one key disagrees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DivergenceKind {
    /// Canonical state has the key, the contract index does not: a mutation reached state without
    /// going through `submit`.
    MissingFromContract,
    /// The contract index has the key, canonical state does not: state was deleted or re-keyed
    /// without the contract recording it.
    StaleInContract,
    /// Both have the key with different values.
    ValueMismatch,
}

impl DivergenceKind {
    pub fn label(self) -> &'static str {
        match self {
            DivergenceKind::MissingFromContract => "missing_from_contract",
            DivergenceKind::StaleInContract => "stale_in_contract",
            DivergenceKind::ValueMismatch => "value_mismatch",
        }
    }
}

/// One disagreeing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    pub key: String,
    pub kind: DivergenceKind,
    pub canonical: Option<String>,
    pub contract: Option<String>,
}

/// The result of one comparison.
#[derive(Clone, Debug, Default)]
pub struct ReconcileReport {
    /// Keys either side can attest that were compared.
    pub compared: usize,
    /// Keys only the contract holds, outside canonical coverage by design (write counters, last
    /// bytes). Reported, never dropped -- an unexpected entry here is how coverage drift shows up.
    pub contract_only: Vec<String>,
    pub divergences: Vec<Divergence>,
}

impl ReconcileReport {
    pub fn in_sync(&self) -> bool {
        self.divergences.is_empty()
    }

    pub fn count(&self, kind: DivergenceKind) -> usize {
        self.divergences.iter().filter(|d| d.kind == kind).count()
    }
}

/// Diff a canonical projection against the contract's index. Pure: no I/O, no mutation, no auth.
///
/// Iteration is over the sorted union of keys, so the report is byte-identical for the same inputs
/// on any thread or process -- the same determinism rule `exec::apply` is held to.
pub fn reconcile(canonical: &Index, contract: &Index) -> ReconcileReport {
    let mut report = ReconcileReport::default();
    let mut keys: Vec<&String> = canonical.keys().chain(contract.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        let c = canonical.get(k);
        let x = contract.get(k);
        match (c, x) {
            (Some(cv), Some(xv)) => {
                report.compared += 1;
                if cv != xv {
                    report.divergences.push(Divergence {
                        key: k.clone(),
                        kind: DivergenceKind::ValueMismatch,
                        canonical: Some(cv.clone()),
                        contract: Some(xv.clone()),
                    });
                }
            }
            (Some(cv), None) => {
                report.compared += 1;
                report.divergences.push(Divergence {
                    key: k.clone(),
                    kind: DivergenceKind::MissingFromContract,
                    canonical: Some(cv.clone()),
                    contract: None,
                });
            }
            (None, Some(xv)) => {
                // Outside canonical coverage by design (a write counter) or a genuine stale row.
                // Distinguished by the coverage predicate, not by guesswork.
                if is_attestable(k) {
                    report.compared += 1;
                    report.divergences.push(Divergence {
                        key: k.clone(),
                        kind: DivergenceKind::StaleInContract,
                        canonical: None,
                        contract: Some(xv.clone()),
                    });
                } else {
                    report.contract_only.push(k.clone());
                }
            }
            (None, None) => {}
        }
    }
    report.divergences.sort_by(|a, b| a.key.cmp(&b.key));
    report.contract_only.sort();
    report
}

/// Whether canonical state can speak to this key at all. Only the liveness key is attestable today;
/// everything else the contract holds is a contract-local fact and is reported as `contract_only`.
pub fn is_attestable(key: &str) -> bool {
    key.ends_with("/status")
}
