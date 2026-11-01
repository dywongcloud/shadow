//! Canonical-state reconciliation: the check that the contract log has not silently diverged from
//! the platform's ONE canonical state.
//!
//! Why this module exists. The contract layer is additive: a database mutation reaches the platform
//! through `crates/hive-cloud/src/admin.rs` and its gates (`tenant()` at `admin.rs:3284`,
//! `require_project` at `admin.rs:3368`, `write_scope` at `admin.rs:13876`) and only records into the
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
//! platform itself stores. For the managed-db contract that is the liveness key
//! (`{tenant}/{db_id}/status`) AND the write plane -- the per-database write count and the last
//! bytes value -- because a `db.write` is a mutation of tenant data and leaving it out of coverage
//! is what let 500 unauthorized writes reconcile as `in_sync` with zero divergences
//! (contract-write-plane-outside-canonical-coverage). [`CanonicalDb`] carries the write plane as
//! `Option`s: when the platform records it, the two sides are compared; when it does not, the
//! contract's claim is reported as a divergence rather than as a contract-local fact nobody checks.

use crate::exec::Index;
use std::collections::BTreeMap;

/// The platform's view of one managed database: the fields canonical state can attest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalDb {
    pub id: String,
    /// The STORED team tag, run through [`crate::contract::record_tenant`] before comparison.
    pub team: String,
    pub project: String,
    pub status: String,
    /// When the record was created. Load-bearing for tombstone adjudication: a tombstone is only the
    /// erasure of the records that existed when it was written, so a record created LATER survives
    /// one. A `CanonicalDb` built without it projects as created at the epoch, which loses to every
    /// tombstone -- fail-closed, never fail-open.
    pub created_ms: u64,
    /// Canonical state's own count of writes to this database: the write plane, which is the part of
    /// a `db.write` this projection used to be silent about. `None` means the platform records no
    /// write count for this database, so any count the contract claims is unattested.
    pub write_count: Option<u64>,
    /// The `bytes` value canonical state recorded for the database's last write. `None` = not
    /// recorded, so a last-bytes value in the contract index is unattested.
    pub last_bytes: Option<String>,
}

/// One declared mutation path that belongs in contract scope.
///
/// `wired: false` means the handler has NOT been given the `Engine::submit` call yet, so a mutation
/// through it is invisible to the log -- exactly the divergence [`reconcile`] exists to surface.
///
/// `line` is a HINT for a human reader, never the identity of the entry: an edit anywhere above a
/// call site in an 18k-line file moves it, and the whole `path:line` list was stale by 40-200 lines
/// before this was fixed -- including on the commit that added realtime, which shifted every
/// downstream entry by 94 lines on its own. [`site`] is the identity, and [`measure`] is what
/// answers "is it wired": `wired` is the CLAIM, `measure` is the measurement of it.
#[derive(Clone, Copy, Debug)]
pub struct ScopeEntry {
    pub op_kind: &'static str,
    pub file: &'static str,
    /// The stable identity of the call site: `fn_name`, or `fn_name#arm` for one arm of a `match`
    /// over the storage lane. Chosen over a line number because it survives every edit that does
    /// not touch the site itself -- and an edit that DOES touch the site is exactly the edit that
    /// should invalidate the entry.
    pub site: &'static str,
    /// The seam call whose presence inside [`site`] IS the wiring: `db_contract::wrote(` etc. A
    /// site that mutates a `databases` row without this token is unwired however it is documented.
    pub seam: &'static str,
    pub line: u32,
    pub wired: bool,
}

/// The mutation surface the managed-db contract claims, located by function identity.
///
/// Every entry is a mutation path that already enforces tenancy; the contract layer records the
/// decision, it does not replace it. `wired` is true only for a path whose handler actually calls
/// `Engine::submit` through `hive-cloud`'s one seam (`src/db_contract.rs`) -- and only from the
/// commit in which that call landed, never in anticipation of it. Every `db.delete` path is wired,
/// including the three that remove a row without a request for it (a project purge, an incarnation
/// discard, an operator console row delete) plus workflow storage that lost its project authority;
/// each of them used to remove a `databases` row in silence, which left the index asserting a
/// database canonical state no longer had.
///
/// Sixteen entries, not ten. The list used to declare ten and print `declared=10 wired=10`, and
/// both numbers were produced by counting this array -- `DECLARED_SCOPE.len()` and a filter over
/// its own `wired` flags -- so `10/10` was arithmetic on a claim, never a reading of the tree. The
/// real surface was twelve at that commit: realtime publish had been wired from two places
/// (`ws_realtime`, and the `realtime` arm of `apply_mirrored_write`) and declared from neither. It
/// is sixteen now because the blob, queue, vector and pub/sub arms of `apply_mirrored_write` were
/// the same omission one layer down -- real mutations of the receiving node's store that recorded
/// nothing, so a write replicated to a peer was visible only on the origin. [`measure`] is what
/// keeps this list honest from here: it reads the tree and reports per site, so a future arm that
/// mutates without the seam shows up as `Unwired` instead of as a number that agrees with itself.
pub const DECLARED_SCOPE: &[ScopeEntry] = &[
    ScopeEntry { op_kind: "db.create", file: "crates/hive-cloud/src/admin.rs", site: "database_create", seam: "db_contract::created(", line: 13219, wired: true },
    ScopeEntry { op_kind: "db.delete", file: "crates/hive-cloud/src/admin.rs", site: "database_delete", seam: "db_contract::deleted(", line: 13654, wired: true },
    // Removals with no request for one database in flight. Recorded under the tenant the STORED
    // record names, because that is the only tenant identity bound to the target and a refusal
    // would apply no mutation -- leaving the index holding a database that is already gone.
    ScopeEntry { op_kind: "db.delete", file: "crates/hive-cloud/src/admin.rs", site: "purge_project_resources", seam: "db_contract::purged(", line: 4833, wired: true },
    ScopeEntry { op_kind: "db.delete", file: "crates/hive-cloud/src/admin.rs", site: "database_ready_callback", seam: "db_contract::purged(", line: 13018, wired: true },
    ScopeEntry { op_kind: "db.delete", file: "crates/hive-cloud/src/admin.rs", site: "data_delete", seam: "db_contract::purged(", line: 17523, wired: true },
    ScopeEntry { op_kind: "db.delete", file: "crates/hive-cloud/src/git.rs", site: "run_build", seam: "db_contract::purged(", line: 3793, wired: true },
    // The five data-plane writes, all of which pass through `write_scope`. Every one of them now
    // records through the seam (`db_contract::wrote`), so a `db.write` moves tenant data WITH an
    // entry in the log -- before this, an entire write plane was invisible to the contract and
    // reconciled as `in_sync` on its own absence.
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "blob_put", seam: "db_contract::wrote(", line: 14119, wired: true },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "queue_push", seam: "db_contract::wrote(", line: 14278, wired: true },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "vector_upsert", seam: "db_contract::wrote(", line: 14414, wired: true },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "pubsub_publish", seam: "db_contract::wrote(", line: 14589, wired: true },
    // Realtime publish, which reaches the seam from two places: the WebSocket handler that
    // receives a client frame, and the mirrored arm that replays a fanned-out frame on a peer.
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "ws_realtime", seam: "db_contract::wrote(", line: 14732, wired: true },
    // The five MIRRORED arms: one per lane, because each is a separate mutation of the receiving
    // node's store with its own measurement. Recorded under `PLATFORM_ACTOR` (a mirrored replay has
    // no request in flight); fan-out excludes this node by name, so nothing is double-recorded.
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "apply_mirrored_write#blob", seam: "db_contract::wrote(", line: 13839, wired: true },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "apply_mirrored_write#queue", seam: "db_contract::wrote(", line: 13869, wired: true },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "apply_mirrored_write#vector", seam: "db_contract::wrote(", line: 13899, wired: true },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "apply_mirrored_write#pubsub", seam: "db_contract::wrote(", line: 13932, wired: true },
    ScopeEntry { op_kind: "db.write", file: "crates/hive-cloud/src/admin.rs", site: "apply_mirrored_write#realtime", seam: "db_contract::wrote(", line: 13970, wired: true },
];

/// Whether one declared site actually reaches the seam, as read from the source tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wiring {
    /// The site's body contains its seam call. `at` is the 1-based line the call was found on, so
    /// a declared line that has drifted is reported rather than silently carried.
    Wired { at: u32 },
    /// The site was located and its body has no seam call: a mutation the log is SILENT about.
    Unwired { reason: &'static str },
    /// The source tree could not be read or the site could not be located, so nothing is claimed.
    /// Unmeasured is never counted as wired.
    Unmeasured { reason: String },
}

impl Wiring {
    pub fn is_wired(&self) -> bool {
        matches!(self, Wiring::Wired { .. })
    }
}

/// One declared site and what the tree says about it.
#[derive(Clone, Debug)]
pub struct SiteMeasurement {
    pub op_kind: &'static str,
    pub file: &'static str,
    pub site: &'static str,
    pub declared_line: u32,
    pub wiring: Wiring,
}

/// The workspace root this build was compiled from: `crates/hive-contract` -> repo root.
///
/// `DECLARED_SCOPE`'s `file` values are repo-relative, so this is the only prefix needed to turn
/// them into readable paths. Nothing here is load-bearing for the platform: when the tree is not
/// there, every site reports [`Wiring::Unmeasured`] and the count is 0, which is the honest answer
/// for "wired" when nothing was measured.
pub fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// Reads the source tree and reports, per declared site, whether the seam call is really there.
///
/// This is the measurement `declared=N wired=M` used to fake by counting its own flags. It resolves
/// each `site` to a function body (or to one `match` arm of it) in the file the entry names and
/// looks for `seam` inside that body -- so a lane that grows a mutation without `db_contract::` is
/// reported `Unwired`, and a rename that moves the call out of the named function is too.
pub fn measure(root: &std::path::Path) -> Vec<SiteMeasurement> {
    let mut out = Vec::new();
    for e in DECLARED_SCOPE {
        let path = root.join(e.file);
        let wiring = match std::fs::read_to_string(&path) {
            Err(err) => Wiring::Unmeasured {
                reason: format!("{}: {err}", path.display()),
            },
            Ok(src) => match site_body(&src, e.site) {
                None => Wiring::Unmeasured {
                    reason: format!("{}: no function body resolved for `{}`", e.file, e.site),
                },
                Some((start_line, body)) => match body.find(e.seam) {
                    None => Wiring::Unwired {
                        reason: "site located, seam call absent",
                    },
                    Some(_) => {
                        let idx = body.find(e.seam).unwrap_or(0);
                        let before = body[..idx].lines().count().saturating_sub(1);
                        let at = start_line as u32 + 1 + before as u32;
                        Wiring::Wired { at }
                    }
                },
            },
        };
        out.push(SiteMeasurement {
            op_kind: e.op_kind,
            file: e.file,
            site: e.site,
            declared_line: e.line,
            wiring,
        });
    }
    out
}

/// Resolves a `fn_name` or `fn_name#arm` site to `(line_index_of_the_returned_body, body_text)`,
/// where the line index is 0-based within the file.
///
/// A top-level `fn` in this workspace ends at a `}` in column 0, which is what bounds the body; an
/// arm is bounded by the next line whose indentation matches the arm's. Deliberately textual: this
/// is a monitor over the source, not a parser, and a site it cannot resolve is reported
/// [`Wiring::Unmeasured`] rather than guessed at.
fn site_body(src: &str, site: &str) -> Option<(usize, String)> {
    let (fn_name, arm) = match site.split_once('#') {
        Some((f, a)) => (f, Some(a)),
        None => (site, None),
    };
    let needle = format!("fn {fn_name}(");
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let declares = t.starts_with("fn ")
            || t.starts_with("pub fn ")
            || t.starts_with("pub(crate) fn ")
            || t.starts_with("async fn ")
            || t.starts_with("pub async fn ")
            || t.starts_with("pub(crate) async fn ");
        if !declares || !t.contains(&needle) {
            continue;
        }
        let mut end = None;
        for (j, l) in lines.iter().enumerate().skip(i + 1) {
            if *l == "}" {
                end = Some(j);
                break;
            }
        }
        let end = end?;
        let body = lines[i..=end].join("\n");
        return Some(match arm {
            None => (i, body),
            Some(a) => {
                let (off, text) = arm_body(&body, a)?;
                (i + off, text)
            }
        });
    }
    None
}

/// Narrows a function body to one `"<arm>" => { ... }` block of its `match`, as
/// `(offset_within_body, arm_text)`.
fn arm_body(body: &str, arm: &str) -> Option<(usize, String)> {
    let needle = format!("\"{arm}\" => {{");
    let lines: Vec<&str> = body.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if !l.trim_end().ends_with(&needle) {
            continue;
        }
        let indent = l.len() - l.trim_start().len();
        for (j, l2) in lines.iter().enumerate().skip(i + 1) {
            let t = l2.trim_start();
            if t.starts_with('}') && l2.len() - t.len() == indent {
                return Some((i, lines[i..=j].join("\n")));
            }
        }
    }
    None
}

/// `(declared, wired, unmeasured)` where `wired` is MEASURED against the source tree.
pub fn scope_measurement() -> (usize, usize, usize) {
    let m = measure(&workspace_root());
    let wired = m.iter().filter(|s| s.wiring.is_wired()).count();
    let unmeasured = m
        .iter()
        .filter(|s| matches!(s.wiring, Wiring::Unmeasured { .. }))
        .count();
    (DECLARED_SCOPE.len(), wired, unmeasured)
}

/// The number `contract-reconcile` prints as `declared=N wired=M`: M is the count of sites the
/// source tree CONFIRMS, not the count of `wired` flags this array carries.
///
/// When the tree is unreachable every site is `Unmeasured` and M is 0 -- reported loudly on stderr,
/// because "0 wired" and "0 measured" are different facts and only one of them is a finding.
pub fn scope_report() -> (usize, usize) {
    let (declared, wired, unmeasured) = scope_measurement();
    if unmeasured > 0 {
        eprintln!(
            "contract: {unmeasured}/{} declared scope sites are UNMEASURED (source tree not readable from {}) -- \
             the wired count below is a measurement of {}/{}, not a coverage claim of 0",
            declared,
            workspace_root().display(),
            declared - unmeasured,
            declared
        );
    }
    (declared, wired)
}

/// The index key canonical state and the contract both speak for one database's liveness.
///
/// Mirrors `exec::apply`'s `db.create` key exactly; if the two ever drift, reconciliation reports a
/// divergence on every row instead of silently agreeing.
pub fn status_key(tenant: &str, target: &str) -> String {
    format!("{}/status", crate::exec::namespace(tenant, target))
}

/// The write-plane keys, mirroring `exec::apply`'s `db.write` keys exactly.
pub fn writes_key(tenant: &str, target: &str) -> String {
    format!("{}/writes", crate::exec::namespace(tenant, target))
}

pub fn last_bytes_key(tenant: &str, target: &str) -> String {
    format!("{}/last_bytes", crate::exec::namespace(tenant, target))
}

/// Project canonical state into contract-index shape.
///
/// `apply`'s inverse over the attestable keys: a database that is present and not tombstoned has a
/// live status key; a tombstoned one (`DatabaseStore::remove_db`, `databases.rs:740`, tombstone
/// written at :742) has none. Tombstones are permanent there ("absence never erases a row" is the
/// row-merge rule; a tombstone is the erasure), so they are the platform's delete-of-record here.
///
/// A tombstone is adjudicated BY TIMESTAMP, not by presence, because that is the platform's own
/// rule: `merge_synced` (`databases.rs:822`) keeps a record whose `created_ms` is LATER than the
/// tombstone's ms, so an id that was deleted and then created again under the same id is live.
/// Skipping every id that appears in the tombstone map -- what this did before -- reported those
/// rows as deleted and produced divergences on live databases: measured on a real snapshot, 2 of 21
/// databases were both present and tombstoned (created 1792720000000, tombstoned 1792715805869),
/// which cost 2 status keys and 6 divergences on state that was in fact in sync.
///
/// The write plane is projected too, and only where the platform actually recorded it: a `None` is
/// left out rather than defaulted to zero, so the reconciler never manufactures a canonical value it
/// was not given -- it reports the contract's claim as unattested instead.
pub fn project(dbs: &[CanonicalDb], tombstones: &BTreeMap<String, u64>) -> Index {
    let mut out = Index::new();
    for d in dbs {
        let tenant = crate::contract::record_tenant(Some(&d.team));
        // Deleted only when the record predates the deletion: the platform's own comparator
        // (`databases.rs:822`, `d.created_ms > *deleted_ms`), applied here rather than re-derived.
        if let Some(deleted_ms) = tombstones.get(&d.id) {
            if d.created_ms <= *deleted_ms {
                continue;
            }
        }
        let value = match d.status.as_str() {
            "ready" | "provisioning" | "live" => "live".to_string(),
            other => other.to_string(),
        };
        out.insert(status_key(&tenant, &d.id), value);
        if let Some(n) = d.write_count {
            out.insert(writes_key(&tenant, &d.id), n.to_string());
        }
        if let Some(b) = &d.last_bytes {
            out.insert(last_bytes_key(&tenant, &d.id), b.clone());
        }
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
    /// Keys only the contract holds, outside canonical coverage by design (a key that is not one of
    /// the managed-db contract's attestable keys). Reported, never dropped -- an unexpected entry
    /// here is how coverage drift shows up.
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
                // An attestable key canonical state does not hold: a genuinely stale row, or a
                // write-plane claim the platform never recorded. Distinguished by the coverage
                // predicate, not by guesswork -- and never treated as "in sync".
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

/// Whether canonical state can speak to this key at all.
///
/// The liveness key AND the write plane: a write count or a last-bytes value is a claim about a
/// database, and canonical state is the only authority that can say whether the database exists to
/// be written to and how many writes it recorded. Covering only `/status` is what let an entire
/// unauthorized write plane reconcile as in sync.
pub fn is_attestable(key: &str) -> bool {
    key.ends_with("/status") || key.ends_with("/writes") || key.ends_with("/last_bytes")
}
