//! Fresh-session admission for low-trust browser serving peers.
//!
//! Browser identities never enter the fleet registry or trusted peer set. The
//! control-plane leader owns this short-lived store; followers adopt versioned
//! snapshots and only use the records to program Gateway's browser target layer.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use fluid_gateway::{BrowserScope, BrowserTarget};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, OnceLock};

use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;
type ApiResult = Result<Json<Value>, (StatusCode, String)>;
type AdmissionResult = Result<Json<Value>, AdmissionFailure>;

#[derive(Debug, Serialize)]
struct AdmissionError {
    code: &'static str,
    message: String,
    retryable: bool,
}

#[derive(Debug)]
struct AdmissionFailure {
    status: StatusCode,
    error: AdmissionError,
}

impl AdmissionFailure {
    fn terminal(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            error: AdmissionError {
                code,
                message: message.into(),
                retryable: false,
            },
        }
    }

    fn retryable(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            error: AdmissionError {
                code,
                message: message.into(),
                retryable: true,
            },
        }
    }
}

impl IntoResponse for AdmissionFailure {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.error }))).into_response()
    }
}

const DEFAULT_LEASE_SECS: u64 = 120;
const MIN_LEASE_SECS: u64 = 30;
const MAX_LEASE_SECS: u64 = 300;
const DEFAULT_SESSION_MAX_AGE_SECS: u64 = 300;
const DEFAULT_CLOCK_SKEW_SECS: u64 = 30;
const TOMBSTONE_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_ADDR_JSON_BYTES: usize = 16 * 1024;
/// How long an explicit revoke also denies the endpoint at the RELAY layer
/// (bn-p2p-revocation-latency's relay-AccessControl half). Deliberately much
/// shorter than `TOMBSTONE_RETENTION_MS`: this is a network-level block on
/// reconnecting, not the admission-store's own bookkeeping retention, and it
/// must not outlive any plausible legitimate re-admission — a permanently
/// denylisted endpoint_id that the SAME browser later re-uses (a fresh tab
/// reload keeps its persisted identity) would be denied forever with no
/// recovery path.
const RELAY_DENYLIST_RETENTION_MS: u64 = 10 * 60 * 1_000;

/// One (deployment, function, digest) triple a browser is authorized to serve
/// (browser-auto-serve-eligible-set). Every field is SERVER-DERIVED at
/// admit/renewal time from the replicated deployment state under the
/// authenticated tenant — the donor names none of them in auto mode, and even
/// in explicit-pin mode the digest comes from the build-stamped descriptor,
/// never from the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserServe {
    pub deployment: String,
    pub function: String,
    /// The descriptor's canonical policy digest — THE wire digest
    /// (`encode_invoke`, the artifact URL, the donor's pin).
    pub digest: String,
}

impl BrowserServe {
    /// A serve entry is only routable when all three identifiers are present.
    /// `fluid_gateway::set_browser_targets` re-checks this independently, so an
    /// incomplete entry structurally cannot become a route even on a peer that
    /// has never heard of this shape.
    fn complete(&self) -> bool {
        !self.deployment.is_empty() && !self.function.is_empty() && !self.digest.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserAdmission {
    pub endpoint_id: String,
    pub addr_json: String,
    /// The deployment this browser's DATABASE grant is pinned to (and, for
    /// pre-`serves` peers, the one serve entry they can see), or EMPTY when
    /// there is neither (browser-node-optional-serve-target). Empty is a
    /// first-class shape, not a degenerate one: a donor whose tenant has no
    /// browser-eligible function at all still joins the mesh, holds a relay
    /// identity and publishes presence — it simply has no serve lane and no
    /// database grant. Kept a plain `String` (never `Option`) so a
    /// pre-upgrade follower parses the replicated snapshot unchanged.
    ///
    /// INVARIANT: whenever `function`/`digest` are non-empty they name a
    /// function OF THIS deployment — the scalar triple is always a coherent
    /// member of [`Self::serve_entries`], never a cross-deployment mixture. A
    /// pre-upgrade follower routes exactly this one entry (a strict subset of
    /// the real set: absent capability, never wrong capability).
    #[serde(default)]
    pub deployment: String,
    /// The function of `deployment` this browser serves, or EMPTY when that
    /// deployment contributes no serve lane (database-only, or no target at
    /// all). NOT the whole answer since browser-auto-serve-eligible-set — see
    /// `serves` below.
    #[serde(default)]
    pub function: String,
    /// The scalar entry's canonical policy digest, or EMPTY when there is no
    /// scalar serve lane. [`Self::serving`] is the ONE predicate every caller
    /// uses; it reads the full set, not this field.
    #[serde(default)]
    pub digest: String,
    /// The COMPLETE set of (deployment, function, digest) triples this browser
    /// is authorized to serve (browser-auto-serve-eligible-set): every
    /// browser-eligible function of every Ready deployment its tenant owns,
    /// re-derived on every renewal so a newly deployed function starts being
    /// served without the node restarting — or exactly one entry when the donor
    /// pinned a target on purpose.
    ///
    /// `serde(default)` for the rollout: a record replicated by a pre-upgrade
    /// leader carries no set, and [`Self::serve_entries`] then falls back to
    /// the scalar triple — one entry, never zero, never a wildcard.
    #[serde(default)]
    pub serves: Vec<BrowserServe>,
    pub tenant: String,
    pub subject: String,
    pub issued_ms: u64,
    pub expires_ms: u64,
    pub revision: u64,
    #[serde(default)]
    pub scope: BrowserScope,
    pub protocol_version: u16,
    /// The server-derived browser-DATABASE grant (bn-browser-fleet-crr-exchange)
    /// — present only when the admitted deployment's descriptor carries a
    /// `browser_db` block (and, for Public scope, `public_read: true`). This
    /// is the replicated grant the CRR exchange re-checks
    /// (`browser_db::resolve_round_grant`); `serde(default)` keeps
    /// pre-upgrade snapshots parsing — absent means NO db capability, the
    /// designed mid-rollout direction (never wrong capability).
    #[serde(default)]
    pub db: Option<BrowserDbGrant>,
}

/// The replicated half of a database grant (contract §3: "grants ride the
/// admission, server-derived and tenant-pinned, dying with the admission
/// lease"). Every field is resolved from the deployment descriptor at
/// admit/renewal time — never from donor input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDbGrant {
    /// The project the database belongs to (database identity IS the
    /// project; the grant is resolved against this exact deployment).
    pub project: String,
    pub access: BrowserDbAccess,
    /// Caps resolved from the spec at issue time — the exchange re-resolves
    /// the LIVE spec per request; these are the snapshot the donor
    /// reconciles its own enforcement from.
    pub max_bytes: u64,
    pub max_value_bytes: u64,
    /// Platform-templated replica name (`browser_db::replica_file_name`) —
    /// the browser opens exactly this OPFS file and nothing derived from any
    /// other input.
    pub db_file: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserDbAccess {
    /// Team scope: the browser may push its changes AND pull fleet changes —
    /// the whole point of a CRR is browsers as writers.
    ReadWrite,
    /// Public scope (only with `public_read: true`): export only; the fleet
    /// applies nothing originating from this grant.
    ReadOnly,
}

impl BrowserAdmission {
    /// The routable serve entries this record authorizes — the ONE resolver
    /// every routing decision funnels through, so a target-less donor can never
    /// acquire a route by an inconsistent check.
    ///
    /// Prefers the replicated set; falls back to the scalar triple for a record
    /// written by a pre-`serves` leader (rollout compat). Incomplete entries are
    /// dropped here as well as refused by the gateway.
    pub fn serve_entries(&self) -> Vec<BrowserServe> {
        if !self.serves.is_empty() {
            return self
                .serves
                .iter()
                .filter(|entry| entry.complete())
                .cloned()
                .collect();
        }
        let scalar = BrowserServe {
            deployment: self.deployment.clone(),
            function: self.function.clone(),
            digest: self.digest.clone(),
        };
        if scalar.complete() {
            vec![scalar]
        } else {
            Vec::new()
        }
    }

    /// Does this admission carry a SERVE lane at all?
    pub fn serving(&self) -> bool {
        !self.serve_entries().is_empty()
    }

    /// Does this admission serve anything of `deployment`? (The join the
    /// per-deployment status view needs, expressed once.)
    pub fn serves_deployment(&self, deployment: &str) -> bool {
        self.serve_entries()
            .iter()
            .any(|entry| entry.deployment == deployment)
    }

    fn targets(&self) -> Vec<BrowserTarget> {
        self.serve_entries()
            .into_iter()
            .map(|entry| BrowserTarget {
                tenant: self.tenant.clone(),
                deployment: entry.deployment,
                function: entry.function,
                endpoint_id: self.endpoint_id.clone(),
                addr_json: self.addr_json.clone(),
                digest: entry.digest,
                expires_ms: self.expires_ms,
                scope: self.scope,
            })
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BrowserAdmissionSnapshot {
    version: u64,
    active: BTreeMap<String, BrowserAdmission>,
    tombstones: BTreeMap<String, u64>,
    /// EXPLICIT revocations only (endpoint_id -> revoked-at ms) -- unlike
    /// `tombstones` above, a plain lease `expire()` never writes here.
    /// Conflating the two would deny relay-level reconnection for every
    /// browser node whose lease simply ran out (the overwhelmingly common,
    /// expected case), not just the ones an operator actually revoked.
    #[serde(default)]
    denylist: BTreeMap<String, u64>,
}

impl BrowserAdmissionSnapshot {
    fn new() -> Self {
        Self {
            version: hive_core::now_ms().max(1),
            active: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            denylist: BTreeMap::new(),
        }
    }

    fn next_version(&mut self) -> u64 {
        self.version = hive_core::now_ms().max(self.version.saturating_add(1));
        self.version
    }

    fn prune_tombstones(&mut self, now: u64) {
        let floor = now.saturating_sub(TOMBSTONE_RETENTION_MS);
        self.tombstones.retain(|_, revision| *revision >= floor);
        let deny_floor = now.saturating_sub(RELAY_DENYLIST_RETENTION_MS);
        self.denylist.retain(|_, revoked_at| *revoked_at >= deny_floor);
    }
}

/// Bounded, tenant-free counters (bn-p2p-observability): global aggregates
/// only, never a per-tenant/per-endpoint breakdown, so exposing them can
/// never leak cardinality or identify any specific browser peer.
#[derive(Default, Serialize)]
pub struct BrowserAdmissionCounters {
    pub admissions_total: u64,
    pub renewals_total: u64,
    pub revocations_total: u64,
    pub expirations_total: u64,
    pub denials_total: u64,
    /// Live size of the relay-level denylist (bn-p2p-revocation-latency) —
    /// bounded (self-prunes on `RELAY_DENYLIST_RETENTION_MS`) and tenant-free,
    /// same shape as every other counter here.
    pub relay_denylist_size: usize,
}

#[derive(Default)]
struct AdmissionCounterCells {
    admissions_total: std::sync::atomic::AtomicU64,
    renewals_total: std::sync::atomic::AtomicU64,
    revocations_total: std::sync::atomic::AtomicU64,
    expirations_total: std::sync::atomic::AtomicU64,
    denials_total: std::sync::atomic::AtomicU64,
}

pub struct BrowserAdmissionStore {
    inner: Mutex<BrowserAdmissionSnapshot>,
    counters: AdmissionCounterCells,
}

impl BrowserAdmissionStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BrowserAdmissionSnapshot::new()),
            counters: AdmissionCounterCells::default(),
        }
    }

    pub fn stats(&self) -> BrowserAdmissionCounters {
        use std::sync::atomic::Ordering::Relaxed;
        let mut state = self.inner.lock();
        state.prune_tombstones(hive_core::now_ms());
        let relay_denylist_size = state.denylist.len();
        drop(state);
        BrowserAdmissionCounters {
            admissions_total: self.counters.admissions_total.load(Relaxed),
            renewals_total: self.counters.renewals_total.load(Relaxed),
            revocations_total: self.counters.revocations_total.load(Relaxed),
            expirations_total: self.counters.expirations_total.load(Relaxed),
            denials_total: self.counters.denials_total.load(Relaxed),
            relay_denylist_size,
        }
    }

    fn list(&self, tenant: &str, now: u64) -> Vec<BrowserAdmission> {
        self.inner
            .lock()
            .active
            .values()
            .filter(|record| record.tenant == tenant && record.expires_ms > now)
            .cloned()
            .collect()
    }

    /// Every live admission across EVERY tenant, ordered by endpoint id.
    ///
    /// The ROLL CALL's inventory view (`roll_call` below) and nothing else: an
    /// operator-only, read-only sweep needs the whole protocol population, not
    /// one tenant's slice, and deriving it by iterating tenants would miss any
    /// record whose tenant contributes no other signal. Deliberately NOT
    /// exposed beyond this module — every tenant-facing reader keeps going
    /// through [`Self::list`]/[`Self::get`], which pin the tenant.
    fn active(&self, now: u64) -> Vec<BrowserAdmission> {
        let mut out: Vec<BrowserAdmission> = self
            .inner
            .lock()
            .active
            .values()
            .filter(|record| record.expires_ms > now)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
        out
    }

    fn get(&self, tenant: &str, endpoint_id: &str, now: u64) -> Option<BrowserAdmission> {
        self.inner
            .lock()
            .active
            .get(endpoint_id)
            .filter(|record| record.tenant == tenant && record.expires_ms > now)
            .cloned()
    }

    fn endpoint_active(&self, endpoint_id: &str, now: u64) -> bool {
        self.inner
            .lock()
            .active
            .get(endpoint_id)
            .is_some_and(|record| record.expires_ms > now)
    }

    /// The LIVE record for an endpoint regardless of tenant — the CRR
    /// exchange's re-check view (`browser_db::resolve_round_grant`). The
    /// endpoint id is the QUIC-authenticated identity, so this leaks nothing
    /// the caller doesn't already prove; tenant pinning is re-checked by the
    /// caller against the record's own fields.
    pub fn live_for_endpoint(&self, endpoint_id: &str, now: u64) -> Option<BrowserAdmission> {
        self.inner
            .lock()
            .active
            .get(endpoint_id)
            .filter(|record| record.expires_ms > now)
            .cloned()
    }

    /// Explicit-revocation check for the embedded relay's `AccessControl`
    /// (bn-p2p-revocation-latency) -- deliberately NOT `endpoint_active`'s
    /// negation. A browser between leases (renewal hasn't landed yet, or it
    /// simply isn't currently admitted to anything) is not "denied"; only an
    /// endpoint an operator actually revoked is.
    pub fn is_denied(&self, endpoint_id: &str, now: u64) -> bool {
        let mut state = self.inner.lock();
        state.prune_tombstones(now);
        state.denylist.contains_key(endpoint_id)
    }

    fn put(&self, mut record: BrowserAdmission) -> Result<Option<BrowserAdmission>, &'static str> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut state = self.inner.lock();
        if let Some(existing) = state.active.get(&record.endpoint_id) {
            if existing.expires_ms > hive_core::now_ms()
                && (existing.tenant != record.tenant || existing.subject != record.subject)
            {
                self.counters.denials_total.fetch_add(1, Relaxed);
                return Err("browser endpoint is owned by another active session");
            }
        }
        let revision = state.next_version();
        record.revision = revision;
        state.tombstones.remove(&record.endpoint_id);
        // A fresh, fully-validated admission (fresh interactive session +
        // proof-of-possession of the endpoint's own key + server-resolved
        // descriptor — every check in `validate_request` already passed by the
        // time `admit` calls this) SUPERSEDES any relay-denylist entry for the
        // same endpoint id (bn-relay-denylist-restart-friction). The denylist
        // exists to keep a REVOKED identity off the relay; once that same
        // identity authenticates a brand-new admission, denying its relay
        // reconnection is no longer revocation enforcement, it is a 10-minute
        // stop-then-start outage for a deliberate restart (`stop()`'s DELETE
        // is itself a revoke). Revocation semantics are untouched: without a
        // new admission the entry still stands for its full retention window,
        // and this changes nothing about WHICH admissions are accepted — only
        // the relay layer learns about an acceptance the store already made.
        state.denylist.remove(&record.endpoint_id);
        let endpoint_id = record.endpoint_id.clone();
        let old = state.active.insert(endpoint_id, record);
        if old.is_some() {
            self.counters.renewals_total.fetch_add(1, Relaxed);
        } else {
            self.counters.admissions_total.fetch_add(1, Relaxed);
        }
        Ok(old)
    }

    /// Fast-path relay deny applied by a follower echoing a leader's revoke
    /// (bn-p2p-revocation-latency, `fanout_revoke`/`mesh_revoke_echo` below) --
    /// deliberately independent of `revoke()`'s full active/tombstone/version
    /// mutation. Touching this node's own wall-clock-anchored version counter
    /// as if IT made the authoritative decision would advertise a logical time
    /// this node never earned (see `adopt`'s version note — a pre-upgrade
    /// follower still gates its wholesale replace on that number). The narrower
    /// operation (denylist only) is safe regardless, and `adopt` re-derives the
    /// entry's fate on every merge: it survives until some node's strictly
    /// newer admission for the same endpoint supersedes it.
    fn mark_denied(&self, endpoint_id: &str, now: u64) {
        let mut state = self.inner.lock();
        state.denylist.insert(endpoint_id.to_string(), now);
        state.prune_tombstones(now);
    }

    /// Fast-path relay UN-deny applied by a follower echoing a leader's fresh
    /// admission (bn-relay-denylist-restart-friction,
    /// `fanout_deny_clear`/`mesh_deny_clear_echo` below) -- the exact inverse
    /// of [`mark_denied`] and bound by the same discipline: denylist only,
    /// never the versioned active/tombstone state, so it cannot race this
    /// follower's own `adopt()` ordering. `put` clears the entry on the
    /// leader; this clears it on followers before the next periodic snapshot
    /// adoption would (otherwise a stop-then-start still waits up to the
    /// store_sync pull interval on whichever follower relay the browser
    /// reconnects to).
    fn mark_deny_cleared(&self, endpoint_id: &str, now: u64) {
        let mut state = self.inner.lock();
        state.denylist.remove(endpoint_id);
        state.prune_tombstones(now);
    }

    fn revoke(&self, tenant: &str, endpoint_id: &str) -> Option<BrowserAdmission> {
        let mut state = self.inner.lock();
        let record = state.active.get(endpoint_id)?;
        if record.tenant != tenant {
            return None;
        }
        let record = state.active.remove(endpoint_id)?;
        let revision = state.next_version();
        state.tombstones.insert(endpoint_id.to_string(), revision);
        state.denylist.insert(endpoint_id.to_string(), hive_core::now_ms());
        state.prune_tombstones(hive_core::now_ms());
        self.counters
            .revocations_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(record)
    }

    fn revoke_team(&self, tenant: &str) -> Vec<BrowserAdmission> {
        let mut state = self.inner.lock();
        let ids: Vec<String> = state
            .active
            .iter()
            .filter(|(_, record)| record.tenant == tenant)
            .map(|(id, _)| id.clone())
            .collect();
        if ids.is_empty() {
            return Vec::new();
        }
        let revision = state.next_version();
        let now = hive_core::now_ms();
        let mut removed = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(record) = state.active.remove(&id) {
                removed.push(record);
                state.tombstones.insert(id.clone(), revision);
                state.denylist.insert(id, now);
            }
        }
        state.prune_tombstones(hive_core::now_ms());
        self.counters
            .revocations_total
            .fetch_add(removed.len() as u64, std::sync::atomic::Ordering::Relaxed);
        removed
    }

    fn expire(&self, now: u64) -> Vec<BrowserAdmission> {
        let mut state = self.inner.lock();
        let ids: Vec<String> = state
            .active
            .iter()
            .filter(|(_, record)| record.expires_ms <= now)
            .map(|(id, _)| id.clone())
            .collect();
        if ids.is_empty() {
            state.prune_tombstones(now);
            return Vec::new();
        }
        let revision = state.next_version();
        let mut removed = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(record) = state.active.remove(&id) {
                removed.push(record);
                state.tombstones.insert(id, revision);
            }
        }
        state.prune_tombstones(now);
        self.counters
            .expirations_total
            .fetch_add(removed.len() as u64, std::sync::atomic::Ordering::Relaxed);
        removed
    }

    fn snapshot(&self) -> BrowserAdmissionSnapshot {
        self.inner.lock().clone()
    }

    /// Merge an incoming snapshot into local state **per endpoint id** — never
    /// as a wholesale replacement.
    ///
    /// This used to be `*state = incoming` behind an `incoming.version >
    /// state.version` gate, which silently DESTROYED admissions — the exact bug
    /// `browser_presence::adopt` was already fixed for, in the store whose
    /// records actually authorize serving. Every node's `version` is wall-clock
    /// anchored (`next_version()` is `now_ms()`), so two nodes that each
    /// admitted a browser hold snapshots whose versions differ by milliseconds;
    /// whichever replicated with the higher version overwrote the other's
    /// entire map, and the browsers admitted through the losing node lost
    /// routing, presence and their database grant with no error logged
    /// anywhere. An admission is a per-endpoint fact owned by whichever node the
    /// browser admitted through — behind round-robin DNS that is routinely a
    /// different node per browser — so the join has to be per endpoint id.
    ///
    /// Rules, applied to the union of both sides:
    /// * a record wins on the higher `revision` (ties keep local);
    /// * a tombstone at or above a record's revision always removes that record,
    ///   whichever side either arrived from, so revocation/expiry still
    ///   propagates as authoritative deletion (the "replicate zero browsers now"
    ///   property `store_sync::REGISTRY` depends on) while a RE-admission, whose
    ///   revision is strictly newer than the tombstone that preceded it,
    ///   survives;
    /// * tombstones union by max revision, so a merge can never lose one;
    /// * the relay denylist unions by max revoked-at and then drops any entry
    ///   the merged `active` map supersedes with a strictly-newer admission —
    ///   `put`'s "a fresh validated admission supersedes the relay denylist; a
    ///   bare revoke still denies" rule (AGENTS.md), enforced on the replication
    ///   path instead of only at the point of admission. Without it a follower's
    ///   own denylist entry for a since-re-admitted endpoint would survive every
    ///   merge and deny that browser at the relay for the full retention window;
    ///   with it, a stale peer snapshot cannot resurrect a cleared entry either,
    ///   because the newer record drops it again in the same pass. A revoke with
    ///   no newer admission leaves no such record, so it keeps denying.
    ///
    /// A record dropped WITHOUT a tombstone (a node that lost state) can be
    /// resurrected here by a peer, deliberately and bounded: leases are minutes
    /// (`MAX_LEASE_SECS`) and every reader filters on `expires_ms`, so a
    /// genuinely dead admission grants nothing and ages out on the leader's next
    /// `expire()` rather than being destroyed early.
    fn adopt(
        &self,
        incoming: BrowserAdmissionSnapshot,
    ) -> Option<(BrowserAdmissionSnapshot, BrowserAdmissionSnapshot)> {
        let mut state = self.inner.lock();
        let old = state.clone();

        for (id, revision) in incoming.tombstones {
            let entry = state.tombstones.entry(id).or_insert(0);
            if revision > *entry {
                *entry = revision;
            }
        }
        for (id, record) in incoming.active {
            if state
                .active
                .get(&id)
                .is_some_and(|local| local.revision >= record.revision)
            {
                continue;
            }
            state.active.insert(id, record);
        }
        // Disjoint field borrows: a tombstone at or above a record's revision
        // removes it, whichever side each of them arrived from.
        let dead: Vec<String> = {
            let BrowserAdmissionSnapshot {
                active, tombstones, ..
            } = &*state;
            active
                .iter()
                .filter(|(id, record)| {
                    tombstones
                        .get(id.as_str())
                        .is_some_and(|revision| *revision >= record.revision)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in dead {
            state.active.remove(&id);
        }

        for (id, revoked_at) in incoming.denylist {
            let entry = state.denylist.entry(id).or_insert(0);
            if revoked_at > *entry {
                *entry = revoked_at;
            }
        }
        // `put`'s supersede rule, applied to the merged state: revisions and
        // denylist stamps are both wall-clock ms from the deciding node, the
        // same basis the tombstone comparison above already rests on, so a
        // record newer than the revoke IS a re-admission that passed fresh
        // session + PoP + descriptor validation on some node.
        let superseded: Vec<String> = {
            let BrowserAdmissionSnapshot {
                active, denylist, ..
            } = &*state;
            denylist
                .iter()
                .filter(|(id, revoked_at)| {
                    active
                        .get(id.as_str())
                        .is_some_and(|record| record.revision > **revoked_at)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in superseded {
            state.denylist.remove(&id);
        }

        // `version` is this node's logical clock for "everything I have ever
        // been told about", and after a merge that is exactly the union of both
        // sides' claims — hence `max`, never a fresh `next_version()` bump. The
        // distinction is load-bearing for pre-upgrade followers, which still
        // adopt wholesale behind `incoming.version > local.version`: stamping
        // `now_ms()` here would advertise a logical time this node never earned
        // and make an old follower REJECT a genuinely newer snapshot from a
        // third node that admitted a browser this one has never seen — data loss
        // manufactured by the fix. `max` can only ever UNDER-claim (holding a
        // superset of the incoming state while advertising its version), which
        // costs at worst one round of staleness on an old follower and is
        // cleared by the next local write, since any admit/renew/revoke bumps
        // the version past it.
        state.version = state.version.max(incoming.version);
        state.prune_tombstones(hive_core::now_ms());

        // Preserve the caller's no-op signal: `adopt_snapshot` treats `None` as
        // "nothing adopted" and skips `reconcile`, so a merge that changed
        // nothing must not be reported as a change.
        if state.active == old.active
            && state.tombstones == old.tombstones
            && state.denylist == old.denylist
        {
            return None;
        }
        let new = state.clone();
        Some((old, new))
    }
}

impl Default for BrowserAdmissionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct AdmissionRequest {
    endpoint_id: String,
    addr_json: String,
    /// OPTIONAL (browser-node-optional-serve-target). Three admissible
    /// shapes, decided entirely server-side in `validate_request`:
    ///   * `deployment` + `function` — an EXPLICIT pin: serve exactly this one
    ///     function (the picker's "pin one on purpose" override).
    ///   * `deployment` alone — database-only: the deployment must resolve
    ///     under the authenticated tenant AND still carry a `browser_db`
    ///     block; no artifact, no serve route, no invoker grants.
    ///   * neither — the automatic shape, decided by `serve_mode` below.
    /// A `function` without a `deployment` is the one rejected combination.
    /// Naming a deployment always OVERRIDES `serve_mode`: an explicit choice is
    /// exactly as narrow as it looks.
    #[serde(default)]
    deployment: String,
    #[serde(default)]
    function: String,
    /// What a donor that named NO deployment wants (browser-auto-serve-eligible-set):
    ///
    ///   * `"auto"` — serve every browser-eligible function this tenant owns,
    ///     re-derived on every renewal. The field is a REQUEST, never a
    ///     capability: the server resolves the set itself from replicated
    ///     deployment state under the authenticated tenant, so asking for
    ///     `auto` can never yield anything the caller was not already entitled
    ///     to (and a caller cannot name what lands in it).
    ///   * anything else, INCLUDING ABSENT — capacity only: no artifact, no
    ///     route, no grants. Absent must keep meaning this: a pre-upgrade
    ///     worker sends no `serve_mode` and relies on an empty target meaning
    ///     "serve nothing", and a rollout must never silently start serving
    ///     code on a donor that did not ask for it.
    #[serde(default)]
    serve_mode: String,
    /// ROLLOUT-COMPATIBILITY ONLY (browser-admission-derived-capabilities):
    /// pre-derived-capability workers believed they chose the code digest and
    /// sent it here. The value is now NEVER consulted — the effective digest
    /// is resolved server-side from the deployment's build-stamped artifact
    /// descriptor, so a forged, stale, or missing donor digest all produce the
    /// same result: the descriptor's canonical policy digest, returned in the
    /// capability block. Kept as an accepted-but-ignored field so a worker
    /// built before this change keeps deserializing during the rollout window.
    #[serde(default)]
    #[allow(dead_code)]
    digest: String,
    #[serde(default)]
    lease_secs: Option<u64>,
    #[serde(default)]
    scope: BrowserScope,
    protocol_version: u16,
    /// Proof-of-possession (bn-p2p-heartbeat-lease): the caller's own
    /// current-time claim, ms since epoch, signed together with
    /// `endpoint_id` — see `signature` below. `#[serde(default)]` so an
    /// older worker (pre-dating this field) still deserializes; it is then
    /// rejected by `validate_request`'s freshness/signature check below,
    /// not by a deserialize failure that would look like a generic 400.
    #[serde(default)]
    challenge_ms: u64,
    /// 128 hex chars: `hive_browser::BrowserNode::signAdmission`'s ed25519
    /// signature over `"{endpoint_id}:{challenge_ms}"`, proving the caller
    /// actually controls the private key for `endpoint_id` — without this,
    /// an admission naming ANY endpoint_id was accepted on the caller's
    /// platform auth alone, with nothing proving they control that
    /// endpoint's key (volunteer-compute-trust-admission-models research,
    /// borrowed from Folding@home's assignment-signature pattern).
    #[serde(default)]
    signature: String,
}

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route("/v1/browser/admissions", get(list_admissions).post(admit))
        .route(
            "/v1/browser/admissions/accept/:endpoint_id",
            get(accept_admission),
        )
        .route(
            "/v1/browser/admissions/:endpoint_id",
            get(get_admission).delete(revoke_admission),
        )
        .route("/v1/browser/stats", get(browser_stats))
        .route("/v1/browser/rollcall", get(roll_call_view))
        // POST, never GET, and the method is the whole design (see
        // `browser_signals`): `admin_ingress` forwards mutations to the
        // control-plane leader and serves GETs LOCALLY behind round-robin DNS,
        // so a mailbox READ over GET would land on an arbitrary node and find
        // an empty box — AGENTS.md's "Round-robin reads vs leader-forwarded
        // writes". Both peers' POSTs converge on the one leader, which is what
        // makes a node-local mailbox correct at all; the read rides the POST's
        // own reply.
        .route("/v1/browser/signals", post(browser_signals))
        .route(
            "/v1/browser/deployments/:id/status",
            get(deployment_status),
        )
}

/// Tenant-scoped "is my browser function actually being served right now"
/// (browser-node-post-deploy-observability): ONE call joining the deployment's
/// descriptors, the live admissions pinned to them, those endpoints' presence
/// state, the artifact host set, and this node's locally-verified byte copy —
/// the join that previously had to be done client-side across three endpoints
/// (and for artifact availability was impossible at all).
async fn deployment_status(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Path(id): Path<String>,
) -> ApiResult {
    let claims = claims_required(claims)?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let now = hive_core::now_ms();

    // Resolve the deployment across local records first, then the gossiped
    // peer view — foreign tenant and unknown id are the identical 404, no
    // existence leak (the resolve_for_tenant precedent).
    let mut found: Option<(String, Vec<fluid_core::BrowserArtifact>)> = None;
    for record in cloud.gw.deployment_records() {
        if record.id != id || crate::admin::record_tenant(&record.tenant) != tenant {
            continue;
        }
        found = Some((
            record.manifest.project.clone(),
            record
                .manifest
                .functions
                .iter()
                .filter_map(|f| f.browser_artifact.clone())
                .collect(),
        ));
        break;
    }
    if found.is_none() {
        let peers = cloud.peer_deployments.read();
        'outer: for (_, deployments) in peers.iter() {
            for info in deployments {
                if info.id.0 == id && crate::admin::record_tenant(&info.tenant) == tenant {
                    found = Some((
                        info.project.clone(),
                        info.browser_functions
                            .iter()
                            .map(|bf| bf.artifact.clone())
                            .collect(),
                    ));
                    break 'outer;
                }
            }
        }
    }
    let Some((project, descriptors)) = found else {
        return Err((StatusCode::NOT_FOUND, "deployment not found".into()));
    };

    let admissions = cloud.browser_admissions.list(&tenant, now);
    let presence = cloud.browser_presence.list(&tenant, now);
    let mut functions = Vec::new();
    for descriptor in &descriptors {
        // The join reads the admission's whole authorized SET, not its scalar
        // compat triple (browser-auto-serve-eligible-set) — an auto-mode donor
        // serving this deployment among several must count here.
        let live: Vec<_> = admissions
            .iter()
            .filter(|a| {
                a.serve_entries().iter().any(|entry| {
                    entry.deployment == id && entry.digest == descriptor.policy_digest
                })
            })
            .collect();
        let online = presence
            .iter()
            .filter(|p| {
                p.state == "online" && live.iter().any(|a| a.endpoint_id == p.endpoint_id)
            })
            .count();
        let artifact_hosts =
            crate::browser_artifacts::resolve_for_tenant(&cloud, &tenant, &descriptor.policy_digest)
                .map(|r| r.hosts)
                .unwrap_or_default();
        let local_verified = crate::browser_artifacts::read_verified(descriptor)
            .await
            .is_some();
        functions.push(json!({
            "policy_digest": descriptor.policy_digest,
            "source_bytes": descriptor.source_bytes,
            "live_admissions": live.len(),
            "presence_online": online,
            "artifact_hosts": artifact_hosts,
            "artifact_local_verified": local_verified,
        }));
    }
    Ok(Json(json!({
        "deployment": id,
        "project": project,
        "browser_functions": functions,
        "admissions": admissions
            .iter()
            .filter(|a| a.serves_deployment(&id) || a.deployment == id)
            .collect::<Vec<_>>(),
    })))
}

/// Bounded, tenant-free operational counters (bn-p2p-observability): global
/// aggregates only across BOTH browser stores plus the BrowserPool's own
/// dial/invoke/byte counters, never a per-tenant or per-endpoint breakdown,
/// so this endpoint structurally cannot leak cardinality or identify any
/// specific browser peer or its location. `pool` is `null` before the first
/// browser-capable iroh endpoint binds (matches `cloud.browser_mesh`'s own
/// `Option` — never fabricated zeros standing in for "not started yet").
async fn browser_stats(State(cloud): State<Arc<CloudState>>, claims: Claims) -> ApiResult {
    crate::admin::require_operator(claims.map(|c| c.0).as_ref())?;
    let pool_stats = cloud.browser_mesh.read().as_ref().map(|pool| pool.stats());
    Ok(Json(json!({
        "admissions": cloud.browser_admissions.stats(),
        "presence": cloud.browser_presence.stats(),
        "pool": pool_stats,
        "signals": signal_stats(),
        // Counters only — the full roster (which names endpoints and tenants)
        // lives behind `/v1/browser/rollcall`, so this endpoint stays the
        // cardinality-free aggregate it was built as. `null` until this node's
        // first sweep, never a fabricated zero.
        "roll_call": last_roll_call()
            .lock()
            .as_ref()
            .map(RollCall::summary)
            .unwrap_or(Value::Null),
    })))
}

fn claims_required(claims: Claims) -> Result<crate::auth::Claims, (StatusCode, String)> {
    claims.map(|claims| claims.0).ok_or((
        StatusCode::UNAUTHORIZED,
        "a verified platform session is required".into(),
    ))
}

fn fresh_user_claims(claims: Claims) -> Result<crate::auth::Claims, AdmissionFailure> {
    let claims = claims_required(claims).map_err(|(_, message)| {
        AdmissionFailure::retryable(StatusCode::UNAUTHORIZED, "session_required", message)
    })?;
    let now = hive_core::now_ms() / 1_000;
    let max_age = std::env::var("HIVE_BROWSER_SESSION_MAX_AGE_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SESSION_MAX_AGE_SECS);
    let skew = std::env::var("HIVE_BROWSER_SESSION_CLOCK_SKEW_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_CLOCK_SKEW_SECS);
    if claims.sub.trim().is_empty()
        || claims.tenant.trim().is_empty()
        || claims.sub.starts_with("key:")
        || claims.role == "service"
    {
        return Err(AdmissionFailure::terminal(
            StatusCode::FORBIDDEN,
            "interactive_session_required",
            "browser admission requires a fresh interactive user session",
        ));
    }
    let iat = claims.iat as u64;
    let exp = claims.exp as u64;
    if exp <= now || iat > now.saturating_add(skew) || now.saturating_sub(iat) > max_age {
        return Err(AdmissionFailure::retryable(
            StatusCode::UNAUTHORIZED,
            "session_stale",
            "browser admission session is expired or not fresh",
        ));
    }
    Ok(claims)
}

/// Divisions (Clerk org tenants) whose MEMBERS — not just owners/admins — may
/// run a PUBLIC browser node. `HIVE_PUBLIC_NODE_TENANTS` (comma-separated,
/// normalized) overrides; defaults to `thoth-division`. A member operating
/// under one of these tenants mints `role: "member"` (Clerk-verified at mint),
/// which the base owner/admin gate would otherwise reject.
fn public_node_tenants() -> Vec<String> {
    let raw = std::env::var("HIVE_PUBLIC_NODE_TENANTS")
        .unwrap_or_else(|_| "thoth-division".to_string());
    raw.split(',')
        .map(|s| crate::admin::norm(s).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// May this caller run a PUBLIC-scope browser node?
///
/// Public serving exposes a deployment to ANY anonymous donor, so it stays
/// gated — but the gate is now three ways, not "team owner/admin" alone:
///   * `platform_admin` — the operator email allowlist (owner_email +
///     HIVE_ADMIN_EMAILS); this is what grants the four named admin emails on
///     whatever tenant they hold.
///   * tenant-scoped `owner`/`admin` — unchanged; every personal namespace
///     mints `owner`, so this already covered a user's own deployments.
///   * membership in a public-node division (see `public_node_tenants`) — the
///     "anyone in thoth division too" grant, keyed on the Clerk-verified
///     `tenant` claim so no email lookup is needed and it can't be spoofed.
fn may_serve_public(claims: &crate::auth::Claims) -> bool {
    if claims.platform_admin || matches!(claims.role.as_str(), "owner" | "admin") {
        return true;
    }
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    public_node_tenants().iter().any(|t| *t == tenant)
}

/// Proof-of-possession (bn-p2p-heartbeat-lease): reject an admission whose
/// `challenge_ms` is stale (bounds replay of a captured signature to this
/// window, rather than forever — there is no separate nonce round trip, so
/// this freshness window IS the replay defense) or whose signature does not
/// verify against `endpoint_id`'s own public key (the endpoint_id string
/// literally IS the ed25519 public key, hex-encoded — see
/// `hive_p2p::endpoint_id_from_addr_json`, which `validate_request` already
/// uses to derive it).
fn verify_proof_of_possession(
    endpoint_id: &str,
    challenge_ms: u64,
    signature_hex: &str,
) -> Result<(), AdmissionFailure> {
    let window_ms = std::env::var("HIVE_BROWSER_POP_WINDOW_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(30_000);
    let now = hive_core::now_ms();
    let age = now.abs_diff(challenge_ms);
    if age > window_ms {
        return Err(AdmissionFailure::retryable(
            StatusCode::UNAUTHORIZED,
            "proof_challenge_stale",
            "browser admission proof-of-possession challenge is stale",
        ));
    }
    let public_key: iroh::PublicKey = endpoint_id.parse().map_err(|_| {
        AdmissionFailure::terminal(
            StatusCode::BAD_REQUEST,
            "endpoint_id_invalid",
            "browser endpoint id is not a valid ed25519 public key",
        )
    })?;
    let mut sig_bytes = [0u8; 64];
    hex::decode_to_slice(signature_hex, &mut sig_bytes).map_err(|_| {
        AdmissionFailure::terminal(
            StatusCode::UNAUTHORIZED,
            "proof_signature_invalid",
            "browser admission proof-of-possession signature is not valid hex",
        )
    })?;
    let signature = iroh::Signature::from_bytes(&sig_bytes);
    let message = format!("{endpoint_id}:{challenge_ms}");
    public_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| {
            AdmissionFailure::terminal(
                StatusCode::UNAUTHORIZED,
                "proof_signature_invalid",
                "browser admission proof-of-possession signature does not verify — caller does not control this endpoint's key",
            )
        })
}

/// One authorized serve entry plus the metadata the capability block carries
/// for it. `project` is display context for the donor's own UI; the
/// authorization is the (deployment, function) → descriptor resolution itself.
struct ServeGrant {
    deployment: String,
    project: String,
    function: String,
    artifact: fluid_core::BrowserArtifact,
}

/// Everything `validate_request` authorized — all server-derived.
struct Authorized {
    tenant: String,
    expires_ms: u64,
    /// The complete serve set (empty = no serve lane).
    serves: Vec<ServeGrant>,
    /// The deployment the DATABASE grant is resolved against, or empty.
    /// Explicit when the donor named one; the tenant's single opted-in project
    /// in auto mode; never an arbitrary pick among several (see
    /// `browser_db::auto_db_deployment_for_tenant`).
    db_deployment: String,
    /// True when the set was derived automatically rather than pinned.
    auto: bool,
}

fn validate_request(
    cloud: &Arc<CloudState>,
    claims: &crate::auth::Claims,
    request: &AdmissionRequest,
) -> Result<Authorized, AdmissionFailure> {
    // Range check, not exact-match (bn-p2p-version-negotiation): the two
    // failure directions need distinct client-facing signals. A durably
    // outdated client (below the server's floor) needs a forced reload — no
    // retry will ever succeed. A client ahead of THIS node (above its
    // ceiling) is the normal mid-rollout shape when other fleet nodes have
    // already rolled forward — transient, worth a bounded retry, never a
    // reload prompt. The exact prefix strings are the wire contract the
    // worker pattern-matches on; changing them is a breaking client change.
    match hive_browser_proto::protocol_fit(request.protocol_version) {
        hive_browser_proto::ProtocolFit::TooOld => {
            return Err(AdmissionFailure::terminal(
                StatusCode::UPGRADE_REQUIRED,
                "protocol_too_old",
                "protocol_too_old: this browser bundle is outdated; reload to update",
            ));
        }
        hive_browser_proto::ProtocolFit::TooNew => {
            return Err(AdmissionFailure::retryable(
                StatusCode::SERVICE_UNAVAILABLE,
                "protocol_too_new",
                "protocol_too_new: this node hasn't rolled forward to your protocol version yet; retrying will reach an upgraded node",
            ));
        }
        hive_browser_proto::ProtocolFit::Supported => {}
    }
    if request.addr_json.len() > MAX_ADDR_JSON_BYTES {
        return Err(AdmissionFailure::terminal(
            StatusCode::PAYLOAD_TOO_LARGE,
            "endpoint_address_too_large",
            "browser endpoint address is too large",
        ));
    }
    let endpoint_id =
        hive_p2p::endpoint_id_from_addr_json(&request.addr_json).ok_or_else(|| {
            AdmissionFailure::terminal(
                StatusCode::BAD_REQUEST,
                "endpoint_address_malformed",
                "browser endpoint address is malformed",
            )
        })?;
    if endpoint_id != request.endpoint_id {
        return Err(AdmissionFailure::terminal(
            StatusCode::BAD_REQUEST,
            "endpoint_id_mismatch",
            "browser endpoint id does not match its signed address",
        ));
    }
    verify_proof_of_possession(&endpoint_id, request.challenge_ms, &request.signature)?;
    // A serve target is OPTIONAL (browser-node-optional-serve-target): running
    // a node and having something browser-servable are independent facts, and
    // a donor whose deployments are all long-running servers/containers can
    // contribute everything that does not need a function artifact. What stays
    // mandatory is COHERENCE — a function name with no deployment names
    // nothing resolvable, and an over-long identifier is malformed input
    // whichever shape it arrives in.
    let deployment = request.deployment.trim();
    let function = request.function.trim();
    if deployment.len() > 256
        || function.len() > 256
        || (deployment.is_empty() && !function.is_empty())
    {
        return Err(AdmissionFailure::terminal(
            StatusCode::BAD_REQUEST,
            "function_target_invalid",
            "invalid browser function target",
        ));
    }
    if request.scope == BrowserScope::Public && !may_serve_public(claims) {
        return Err(AdmissionFailure::terminal(
            StatusCode::FORBIDDEN,
            "public_scope_forbidden",
            "public browser serving requires a platform admin, a team owner/admin, or membership in a public-node division",
        ));
    }
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    // The ENTIRE authorization decision is the server-side descriptor
    // resolution (browser-admission-derived-capabilities, on top of
    // browser-function-artifact-build-contract's store): the deployment +
    // function named by the donor must resolve — under the AUTHENTICATED
    // tenant — to a Ready deployment whose build stamped a browser artifact
    // descriptor on that function. The donor's own `digest` field is a
    // rollout-compat leftover and is never consulted: a forged digest admits
    // nothing (there is nothing to match it against), and a stale one simply
    // gets reconciled to the current descriptor returned in the capability.
    //
    // With NO function named, that resolution is simply skipped and the
    // admission carries no artifact capability at all — never a permissive
    // one. A database-only admission (deployment, no function) still has to
    // resolve its deployment under this tenant, so a serve-less shape can
    // never be used to pin an admission record to a deployment the caller
    // does not own.
    //
    // With no DEPLOYMENT named at all and `serve_mode: "auto"`, the same
    // resolution runs over the tenant's WHOLE eligible set
    // (browser-auto-serve-eligible-set): `eligible_for_tenant` reads exactly
    // the replicated deployment state `descriptor_for` reads, filtered to
    // Ready + this authenticated tenant + a build-stamped artifact, so every
    // member is authorized by construction and the donor named none of them.
    let auto = deployment.is_empty() && request.serve_mode.trim() == "auto";
    let mut db_deployment = deployment.to_string();
    let serves: Vec<ServeGrant> = if !function.is_empty() {
        match crate::browser_artifacts::descriptor_for(cloud, &tenant, deployment, function) {
            None => {
                return Err(AdmissionFailure::retryable(
                    StatusCode::NOT_FOUND,
                    "deployment_not_ready",
                    "no ready deployment function exists in this tenant",
                ));
            }
            Some(None) => {
                return Err(AdmissionFailure::terminal(
                    StatusCode::FORBIDDEN,
                    "function_not_browser_eligible",
                    "the named function has no build-produced browser artifact — it never opted in \
                     via fluid.json `browser`, or the build rejected it as unsupported",
                ));
            }
            Some(Some(descriptor)) => vec![ServeGrant {
                deployment: deployment.to_string(),
                // The project label is display context only; an explicit pin
                // resolves it from the same records `descriptor_for` walked.
                project: project_of(cloud, &tenant, deployment),
                function: descriptor.name,
                artifact: descriptor.artifact,
            }],
        }
    } else if auto {
        // An EMPTY eligible set is not an error (browser-node-optional-serve-target):
        // a tenant whose deployments are all servers/containers gets a live
        // admission with no serve lane, exactly like an explicit "serve
        // nothing" — and starts serving the moment something eligible lands,
        // on the next renewal, with no restart.
        db_deployment =
            crate::browser_db::auto_db_deployment_for_tenant(cloud, &tenant, &endpoint_id)
                .unwrap_or_default();
        crate::browser_artifacts::eligible_for_tenant(cloud, &tenant)
            .into_iter()
            .map(|e| ServeGrant {
                deployment: e.deployment,
                project: e.project,
                function: e.function,
                artifact: e.artifact,
            })
            .collect()
    } else {
        Vec::new()
    };
    // An admission that serves NOTHING and names a deployment is the
    // database-only shape: that deployment must resolve under this tenant AND
    // still carry a block, or the target is dead. Unknown deployment, foreign
    // tenant and block-removed are the IDENTICAL answer (the
    // `db_descriptor_for` contract) — no existence leak across tenants. An
    // AUTO-resolved id can never fail here (it came from a block); a pinned
    // FUNCTION whose deployment has no block never reaches here either (the
    // serve lane stands, there is simply no db grant).
    if serves.is_empty()
        && !db_deployment.is_empty()
        && crate::browser_db::db_descriptor_for(cloud, &tenant, &db_deployment).is_none()
    {
        return Err(AdmissionFailure::retryable(
            StatusCode::NOT_FOUND,
            "deployment_not_ready",
            "no ready deployment with a browser database block exists in this tenant",
        ));
    }
    let now = hive_core::now_ms();
    let requested = request
        .lease_secs
        .unwrap_or(DEFAULT_LEASE_SECS)
        .clamp(MIN_LEASE_SECS, MAX_LEASE_SECS);
    let token_expiry = (claims.exp as u64).saturating_mul(1_000);
    let expires_ms = now
        .saturating_add(requested.saturating_mul(1_000))
        .min(token_expiry);
    if expires_ms <= now.saturating_add(MIN_LEASE_SECS.saturating_mul(1_000)) {
        return Err(AdmissionFailure::retryable(
            StatusCode::UNAUTHORIZED,
            "session_lease_too_short",
            "platform session expires before the minimum browser lease",
        ));
    }
    Ok(Authorized {
        tenant,
        expires_ms,
        serves,
        db_deployment,
        auto,
    })
}

/// The project label of a deployment under `tenant`, for capability display
/// context only (never an authorization input) — empty when the replicated
/// view does not name it.
fn project_of(cloud: &Arc<CloudState>, tenant: &str, deployment: &str) -> String {
    for record in cloud.gw.deployment_records() {
        if record.id == deployment && crate::admin::record_tenant(&record.tenant) == tenant {
            return record.project;
        }
    }
    for deployments in cloud.peer_deployments.read().values() {
        for info in deployments {
            if info.id.0 == deployment && crate::admin::record_tenant(&info.tenant) == tenant {
                return info.project.clone();
            }
        }
    }
    String::new()
}

async fn admit(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Json(request): Json<AdmissionRequest>,
) -> AdmissionResult {
    let claims = fresh_user_claims(claims)?;
    let authorized = validate_request(&cloud, &claims, &request)?;
    let Authorized {
        tenant,
        expires_ms,
        serves,
        db_deployment: deployment,
        auto,
    } = authorized;
    // Server-derived DATABASE grant (bn-browser-fleet-crr-exchange): present
    // only when the admitted deployment's descriptor carries a `browser_db`
    // block — resolved from the same replicated deployment state as the
    // function descriptors, never from donor input. Public scope gets a grant
    // only with `public_read: true`, and even then read-only. Its absence is
    // not an error: function-serving admissions without a database opt-in
    // simply carry no `db` capability, and a donor whose tenant has no (or
    // more than one) opted-in project resolves to `None` by construction.
    let db_capability = crate::browser_db::db_descriptor_for(&cloud, &tenant, &deployment)
        .and_then(|(project, spec)| {
            let resolved = spec.resolve();
            let access = match request.scope {
                BrowserScope::Team => BrowserDbAccess::ReadWrite,
                BrowserScope::Public if resolved.public_read => BrowserDbAccess::ReadOnly,
                BrowserScope::Public => return None,
            };
            let grant = BrowserDbGrant {
                db_file: crate::browser_db::replica_file_name(&project),
                project,
                access,
                max_bytes: resolved.max_bytes,
                max_value_bytes: resolved.max_value_bytes,
            };
            Some((grant, resolved))
        });
    let issued_ms = hive_core::now_ms();
    // Captured BEFORE `put` clears it (bn-relay-denylist-restart-friction):
    // whether this endpoint carried a stale relay-denylist entry (a `stop()`'s
    // DELETE is a revoke). `put` clears the local entry; followers need the
    // echo below to clear theirs before the next snapshot adoption.
    let had_deny_entry = cloud
        .browser_admissions
        .is_denied(&request.endpoint_id, issued_ms);
    // SERVER-DERIVED (browser-admission-derived-capabilities): every entry's
    // digest is the canonical policy digest of a build-stamped descriptor —
    // never the donor-supplied compat field. On a RENEWAL after a redeploy
    // rotated an artifact, this is where the record atomically moves to the new
    // digest (routing_identity_changed below tears the old route down first, so
    // no invoke can straddle them), and where a newly deployed function
    // APPEARS with no restart.
    let serve_entries: Vec<BrowserServe> = serves
        .iter()
        .map(|grant| BrowserServe {
            deployment: grant.deployment.clone(),
            function: grant.function.clone(),
            digest: grant.artifact.policy_digest.clone(),
        })
        .collect();
    // The scalar triple is the COMPAT view for a pre-`serves` follower, and it
    // must stay a COHERENT member of the set: the entry belonging to the
    // database-pinned deployment when there is one, otherwise nothing at all.
    // A mixed (deployment-A, function-of-B) scalar would register a route under
    // a key that could, for a same-named function, execute B's digest for A —
    // so it is never constructed. In auto mode with no database pin the scalar
    // is empty and a pre-upgrade follower simply routes nothing for this
    // endpoint: a strict subset, absent capability rather than wrong.
    let scalar = serve_entries
        .iter()
        .find(|entry| entry.deployment == deployment)
        .cloned()
        .unwrap_or(BrowserServe {
            deployment: deployment.clone(),
            function: String::new(),
            digest: String::new(),
        });
    let mut record = BrowserAdmission {
        endpoint_id: request.endpoint_id,
        addr_json: request.addr_json,
        deployment: scalar.deployment,
        function: scalar.function,
        digest: scalar.digest,
        serves: serve_entries,
        tenant,
        subject: claims.sub,
        issued_ms,
        expires_ms,
        revision: 0,
        scope: request.scope,
        protocol_version: request.protocol_version,
        db: db_capability
            .as_ref()
            .map(|(grant, _)| grant.clone()),
    };
    let old = cloud
        .browser_admissions
        .put(record.clone())
        .map_err(|message| {
            AdmissionFailure::terminal(StatusCode::CONFLICT, "endpoint_conflict", message)
        })?;
    record = cloud
        .browser_admissions
        .get(&record.tenant, &record.endpoint_id, issued_ms)
        .expect("browser admission was just inserted");
    if old
        .as_ref()
        .is_some_and(|old| routing_identity_changed(old, &record))
    {
        // Remove the old route before the async close. Otherwise an invocation
        // in this window can reuse the old BrowserPool trunk with the new grant.
        remove_endpoint(&cloud, &record.endpoint_id).await;
    }
    if record.serving() {
        // ONE atomic set-replace for the endpoint: every entry authorized by
        // this admission becomes routable together, and anything the previous
        // lease had that this one does not is gone in the same write. A
        // serve-less admission is a live, fully-authorized identity with NO
        // route — never a route with a permissive target — and the explicit
        // removal (rather than merely skipping the upsert) keeps that true even
        // if an earlier lease for this same endpoint id had one.
        if let Err(error) = cloud
            .gw
            .set_browser_targets(&record.endpoint_id, record.targets())
        {
            cloud
                .browser_admissions
                .revoke(&record.tenant, &record.endpoint_id);
            return Err(AdmissionFailure::retryable(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_unavailable",
                error,
            ));
        }
    } else {
        cloud.gw.remove_browser_endpoint(&record.endpoint_id);
    }
    if had_deny_entry {
        // The admission is fully committed (store + gateway route) — shrink
        // the window a stop-then-start browser is still denied at FOLLOWER
        // relays from the ~60s snapshot-pull interval to one mesh round trip,
        // exactly mirroring fanout_revoke's latency argument for the deny
        // direction. Best-effort: a missed peer converges on the next pull.
        fanout_deny_clear(&cloud, &record.endpoint_id);
    }
    // The capability block is one ATOMIC snapshot (admit and renewal alike):
    // the FULL descriptor set the server just authorized plus the CURRENT
    // trusted caller set. The donor reconciles its grants from exactly this —
    // pinning every listed artifact, unpinning every one no longer listed,
    // granting every listed caller and revoking every caller no longer listed —
    // so a new deployment, a rotated artifact and a fleet membership change all
    // take effect on the same response, never piecemeal.
    Ok(Json(json!({
        "admission": record,
        "capability": capability_json(
            &cloud,
            &serves,
            auto,
            expires_ms,
            db_capability
                .as_ref()
                .map(|(grant, resolved)| (record.tenant.as_str(), grant, resolved)),
            MeshSelf {
                tenant: &record.tenant,
                endpoint_id: &record.endpoint_id,
                scope: record.scope,
                project: record
                    .db
                    .as_ref()
                    .map(|grant| grant.project.as_str())
                    .unwrap_or_default(),
            },
        ),
    })))
}

/// The server-derived capability returned by `admit` (initial and renewal):
/// everything the donor needs to pin its artifacts and program its invoker
/// grants, and nothing it could have supplied itself.
///
/// * `artifacts[]` — the COMPLETE authorized descriptor set
///   (browser-auto-serve-eligible-set), each entry carrying:
///   * `artifact_url` — the tenant-authorized content-addressed GET
///     (browser-function-artifact-delivery), relative to the API origin the
///     admission call was made against. The worker fetches with a bounded
///     deadline and recomputes BOTH BLAKE3 digests before pinning.
///   * `policy_digest` / `source_digest` / `source_bytes` — verbatim from the
///     build-stamped descriptor, so a stale or mismatched served body is
///     detectable byte-for-byte.
///   * `deployment` / `function` / `project` — display context for the donor's
///     own status UI. NEVER an authorization input on either side: the fleet
///     routes on (deployment, function) from its own admission record, and the
///     donor executes on the policy digest alone.
/// * `trusted_callers` — see [`trusted_caller_ids`]: the exact fleet
///   EndpointIds the donor grants via `BrowserNode.grantInvoker`. Anything
///   else (a wildcard, a browser id, a client-supplied id) is refused by
///   construction — it never appears here.
///
/// An EMPTY set is the target-less/database-only donor
/// (browser-node-optional-serve-target), and now also the auto-mode donor whose
/// tenant has nothing browser-eligible. The block then carries `serving:
/// false`, NO artifact fields at all, and an EMPTY `trusted_callers` list —
/// absence of capability, never a broader one. There is nothing for the donor
/// to pin and therefore nobody it can grant: `grantInvoker` takes a
/// (caller, policy_digest) pair, and a donor with no pinned digest cannot
/// form one.
///
/// ROLLOUT: the FIRST entry is ALSO mirrored into the flat `artifact_url`/
/// `policy_digest`/… fields the pre-set worker reads, so a worker built before
/// `artifacts[]` existed keeps serving one function instead of throwing on a
/// capability it cannot parse. (It then serves a subset of what the fleet
/// routes to it; the extra routes fail its own not-pinned-locally check, open
/// their per-digest circuit, and fall through to normal function serving until
/// the tab reloads onto the current bundle.)
fn capability_json(
    cloud: &Arc<CloudState>,
    serves: &[ServeGrant],
    auto: bool,
    expires_ms: u64,
    db: Option<(&str, &BrowserDbGrant, &fluid_core::ResolvedBrowserDbPolicy)>,
    me: MeshSelf<'_>,
) -> Value {
    let artifact_json = |grant: &ServeGrant| {
        let a = &grant.artifact;
        json!({
            "deployment": grant.deployment,
            "function": grant.function,
            "project": grant.project,
            "artifact_url": format!("/v1/browser/artifacts/{}", a.policy_digest),
            "policy_digest": a.policy_digest,
            "source_digest": a.source_digest,
            "source_bytes": a.source_bytes,
            "mode": a.mode,
            "timeout_ms": a.timeout_ms,
            "memory_bytes": a.memory_bytes,
            "stack_bytes": a.stack_bytes,
            "allowed_ops": a.allowed_ops,
        })
    };
    let mut out = json!({
        "version": 2,
        "serving": !serves.is_empty(),
        "serve_mode": if auto { "auto" } else { "pinned" },
        "artifacts": serves.iter().map(artifact_json).collect::<Vec<_>>(),
        "trusted_callers": if serves.is_empty() {
            Vec::new()
        } else {
            trusted_caller_ids(cloud)
        },
        "expires_ms": expires_ms,
    });
    if let Some(grant) = serves.first() {
        let artifact = &grant.artifact;
        out["artifact_url"] = json!(format!("/v1/browser/artifacts/{}", artifact.policy_digest));
        out["policy_digest"] = json!(artifact.policy_digest);
        out["source_digest"] = json!(artifact.source_digest);
        out["source_bytes"] = json!(artifact.source_bytes);
        out["mode"] = json!(artifact.mode);
        out["timeout_ms"] = json!(artifact.timeout_ms);
        out["memory_bytes"] = json!(artifact.memory_bytes);
        out["stack_bytes"] = json!(artifact.stack_bytes);
        out["allowed_ops"] = json!(artifact.allowed_ops);
    }
    // The `db` section is part of the SAME atomic capability snapshot
    // (bn-browser-fleet-crr-exchange): the donor reconciles its OPFS replica
    // name, caps, schema and sync peers from exactly this — present only when
    // the admission carries a database grant at all.
    if let Some((tenant, grant, resolved)) = db {
        out["db"] = crate::browser_db::capability_db_json(cloud, tenant, grant, resolved, expires_ms);
    }
    // The browser↔browser DIRECT lane's peer set (bn-browser-peer-webrtc-mesh)
    // rides the SAME atomic snapshot, for the same reason `db` and
    // `trusted_callers` do: a peer grant is live server state, and the donor
    // reconciles its whole session set from one response rather than
    // piecemeal. Absent block = no mesh at all (what every pre-upgrade node
    // answers, and what a fleet with `HIVE_BROWSER_MESH=0` answers), which
    // run-node-worker.js already treats as "tear the lane down and keep the
    // relay path".
    if let Some(mesh) = mesh_capability_json(cloud, me, hive_core::now_ms()) {
        out["mesh"] = mesh;
    }
    out
}

/// The fleet EndpointIds currently allowed to originate BrowserPool invokes
/// against an admitted browser: every HEALTHY node in the live registry,
/// keyed by its proven iroh identity — parsed out of the gossiped
/// `EndpointAddr` when present (the canonical form the mesh join verified
/// against the peer's QUIC handshake), else the join-verified `peer_id`.
///
/// Deliberately NOT: client input (the request never names a caller), node
/// NAMES (labels, not identities — the PeerPool keying incident), browser ids
/// (donors must never appear here), or any wildcard/TrustSet aggregate
/// (each entry is one exact EndpointId, which is the only shape
/// `grantInvoker` accepts). Health-filtered because a node the control plane
/// currently cannot reach has no business holding a fresh grant; renewal
/// re-derives the set, so a flapping node loses and regains its grant on the
/// same snapshot the descriptor rotation rides.
fn trusted_caller_ids(cloud: &Arc<CloudState>) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for n in cloud.registry.nodes() {
        if !n.healthy {
            continue;
        }
        let id = n
            .iroh_addr
            .as_deref()
            .and_then(hive_p2p::endpoint_id_from_addr_json)
            .or_else(|| n.peer_id.clone());
        if let Some(id) = id.filter(|id| hive_browser_proto::valid_blake3_digest(id)) {
            ids.insert(id);
        }
    }
    ids.into_iter().collect()
}

// ---------------------------------------------------------------------------
// The browser↔browser DIRECT lane (bn-browser-peer-webrtc-mesh): server half.
//
// Two arms, both strictly additive to the iroh relay path:
//   * the `mesh` capability block — WHO a donor may address, derived here from
//     live admissions under the authenticated tenant, exactly like
//     `trusted_callers` and `db.sync_peers`;
//   * `POST /v1/browser/signals` — a bounded, authenticated mailbox carrying
//     offer/answer/ice/bye envelopes between two ALREADY-ADMITTED endpoints.
//
// The mailbox moves opaque JSON strings and nothing else. It cannot read a
// peer's SDP into anything, and it deliberately CANNOT substitute one: the
// envelope's DTLS fingerprint is signed by the sender's ed25519 endpoint key
// (`hive_browser::sign_mesh_envelope`) and verified by the receiver against
// the endpoint id THIS server admitted, so a compromised mailbox can drop or
// delay a handshake but never sit in the middle of one. That is why the
// signalling surface is allowed to be a plain HTTP arm at all.
//
// Enumeration is impossible by construction: a donor never names a peer. It
// receives a server-derived set, and `to` is refused unless it is a member of
// THAT set, re-derived from live state on every single call — never from the
// caller's cached capability, never from client input.
// ---------------------------------------------------------------------------

/// Default peer-set ceiling per donor. A browser holds one RTCPeerConnection +
/// DataChannel per peer, so this is a real per-tab resource bound, not a
/// formality; `fluid_gateway::MAX_BROWSER_TARGETS_PER_ENDPOINT` has the same
/// shape for routes. peer-mesh.js caps at 32 independently.
const MESH_MAX_PEERS: usize = 16;
const MESH_PEER_HARD_CAP: usize = 32;
const MESH_ARTIFACTS_PER_PEER: usize = 64;
const MESH_PROTOCOL_VERSION: u16 = 1;

/// Mailbox bounds. Every one of them is enforced per CALL as well as per
/// endpoint, so a donor cannot convert the signalling arm into storage.
const SIGNAL_TTL_MS: u64 = 60_000;
const SIGNAL_MAX_PER_ENDPOINT: usize = 32;
const SIGNAL_MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const SIGNAL_MAX_SEND_PER_CALL: usize = 16;
const SIGNAL_MAX_DELIVER_PER_CALL: usize = 16;
/// Hard ceiling on inboxes held at once. Reached only by a fleet with more
/// live browser peers than this; the least-recently-touched box is evicted,
/// which costs its owner one handshake retry and never an admission.
const SIGNAL_MAX_ENDPOINTS: usize = 4096;

/// Is the direct lane offered at all? `HIVE_BROWSER_MESH=0` turns it off
/// fleet-wide: the capability block disappears and the signalling arm answers
/// 501 — which is EXACTLY the shape peer-mesh.js already treats as "this fleet
/// has no signalling arm yet" (dormant lane, relay path untouched), so the
/// kill switch and the pre-upgrade rollout state are the same code path on the
/// donor. Never a new client-visible failure mode.
fn mesh_enabled() -> bool {
    !matches!(
        std::env::var("HIVE_BROWSER_MESH").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// The per-donor degree ceiling. Floored at 2 because the topology below is a
/// symmetric ring: it spends the budget as `cap/2` successors plus `cap/2`
/// predecessors, and a budget of 1 cannot be split symmetrically.
fn mesh_max_peers() -> usize {
    std::env::var("HIVE_BROWSER_MESH_MAX_PEERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= 2)
        .unwrap_or(MESH_MAX_PEERS)
        .min(MESH_PEER_HARD_CAP)
}

/// How many peers ONE member of an `n`-browser tenant is authorized to
/// address, given the degree ceiling. One formula, two callers (the capability
/// derivation and the roll call's roster) — a drift between them would make
/// the roll call report a fan-out the fleet does not actually grant.
fn mesh_degree(n: usize, cap: usize) -> usize {
    let others = n.saturating_sub(1);
    if others <= cap {
        others
    } else {
        (cap / 2) * 2
    }
}

fn mesh_poll_ms() -> u64 {
    std::env::var("HIVE_BROWSER_MESH_POLL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value >= 500)
        .unwrap_or(1_500)
}

/// ICE servers offered to donors — OFF by default, deliberately.
///
/// The same rule the DNS geo path carries (AGENTS.md: "never re-introduce a
/// default endpoint"): shipping a default STUN/TURN URL would put a third
/// party on the connection path of every browser node in the fleet, whether or
/// not the operator wanted one. With none configured the lane still works
/// wherever host/server-reflexive candidates suffice (same LAN, open NAT) and
/// simply fails to connect elsewhere — which is the designed degradation, not
/// an outage: every consumer falls back to the relay.
///
/// `HIVE_BROWSER_ICE_SERVERS` takes either a JSON array of RTCIceServer
/// objects (the only form that can carry TURN credentials) or a plain
/// comma-separated URL list. A MALFORMED value yields NO servers rather than a
/// partially-parsed one — peer-mesh.js re-validates every URL and caps the
/// list again on receipt, so this is the first of two independent filters.
fn mesh_ice_servers() -> Vec<Value> {
    let raw = std::env::var("HIVE_BROWSER_ICE_SERVERS").unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    if raw.starts_with('[') {
        return match serde_json::from_str::<Value>(raw) {
            Ok(Value::Array(items)) => items
                .into_iter()
                .filter(|item| item.get("urls").is_some())
                .take(8)
                .collect(),
            _ => {
                tracing::warn!("HIVE_BROWSER_ICE_SERVERS is not a JSON array — offering none");
                Vec::new()
            }
        };
    }
    raw.split(',')
        .map(str::trim)
        .filter(|url| {
            url.starts_with("stun:") || url.starts_with("turn:") || url.starts_with("turns:")
        })
        .take(8)
        .map(|url| json!({ "urls": [url] }))
        .collect()
}

/// The requesting donor's own identity, as the mesh derivation needs it. A
/// struct rather than four positional args because every field is an
/// authorization input and mixing two of them up is a capability bug.
#[derive(Clone, Copy)]
struct MeshSelf<'a> {
    tenant: &'a str,
    endpoint_id: &'a str,
    scope: BrowserScope,
    /// The database project THIS donor holds a grant for, or empty. Used only
    /// to decide whether a peer's db lane is even relevant — a browser holds
    /// ONE replica, so a peer replicating a different project is a peer with
    /// no database relationship to us at all.
    project: &'a str,
}

/// One authorized peer, fully server-derived.
struct MeshPeerView {
    endpoint_id: String,
    scope: BrowserScope,
    project: String,
    /// The peer's effective database access FROM THIS DONOR'S POINT OF VIEW —
    /// `"none"` unless both sides are Team scope on the same project.
    db: &'static str,
    artifacts: Vec<String>,
}

/// The set of browser endpoints `me` may address, re-derived from LIVE
/// admissions on every call (capability issue AND every signalling round
/// trip). Same-tenant only, self excluded.
///
/// THE MEMBERSHIP RULE IS SYMMETRIC BY CONSTRUCTION, and that is load-bearing
/// rather than tidy. Below the degree ceiling every member simply addresses
/// every other. ABOVE it, the naive "sort and take the first N" is NOT
/// symmetric — with 20 browsers and a ceiling of 16, the 18th id holds the
/// 16th in its set while the 16th does not hold the 18th, so the 18th offers
/// into a mailbox whose owner is required to drop it, forever, on a backoff
/// that never converges. So the overflow rule is a RING: the sorted endpoint
/// ids form a cycle and each member takes `cap/2` successors and `cap/2`
/// predecessors, which makes "B is in A's set" and "A is in B's set" the same
/// statement (`B` is `d` forward of `A` exactly when `A` is `d` back of `B`)
/// and still leaves the tenant's peers one connected component. Every node
/// computes it from the identical replicated admission list, so no agreement
/// protocol is needed — the same reasoning `inference`'s coordinator election
/// uses.
///
/// The database dimension is deliberately narrower than the fleet lane:
///   * a Public-scope peer is ALWAYS `"none"`, and so is every peer when the
///     CALLER is Public scope. Public scope exists so an anonymous donor can
///     serve functions; the fleet remains the system of record for
///     `public_read` data, and a browser↔browser export is a lane the tenant
///     never opted into. Absent capability beats a defensible one.
///   * projects must match. A browser holds exactly one replica
///     (`browser_db::auto_db_deployment_for_tenant` refuses to pick among
///     several for the same reason), so a cross-project db lane could only
///     ever be a mis-addressed round.
/// Function artifacts have no such restriction: they are content-addressed and
/// re-verified byte-for-byte by the receiver (`WorkerFunctionRuntime.pin`
/// recomputes size + BLAKE3 + the canonical policy digest), so the worst a
/// peer can do with a digest it holds is waste a round trip.
fn mesh_peers(cloud: &Arc<CloudState>, me: MeshSelf<'_>, now: u64) -> Vec<MeshPeerView> {
    let team_scoped_self = me.scope == BrowserScope::Team;
    let mut records = cloud.browser_admissions.list(me.tenant, now);
    records.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
    let n = records.len();
    // Self must be in the list — `admit` puts the record before building the
    // capability, and the signalling arm resolves the caller from this very
    // store. If it somehow is not, the ring has no well-defined origin, and an
    // arbitrary origin would be exactly the asymmetry this rule exists to
    // prevent: answer "no peers" rather than a set the other half disagrees
    // with.
    let Some(index) = records
        .iter()
        .position(|record| record.endpoint_id == me.endpoint_id)
    else {
        return Vec::new();
    };
    let cap = mesh_max_peers();
    let chosen: Vec<usize> = if n.saturating_sub(1) <= cap {
        (0..n).filter(|i| *i != index).collect()
    } else {
        let half = cap / 2;
        let mut ring: BTreeSet<usize> = BTreeSet::new();
        for step in 1..=half {
            ring.insert((index + step) % n);
            ring.insert((index + n - step) % n);
        }
        ring.remove(&index);
        ring.into_iter().collect()
    };
    let mut peers: Vec<MeshPeerView> = chosen
        .into_iter()
        .map(|i| records[i].clone())
        .map(|record| {
            let grant = record.db.as_ref();
            let project = grant.map(|g| g.project.clone()).unwrap_or_default();
            let db = match (team_scoped_self, record.scope, grant) {
                (true, BrowserScope::Team, Some(g))
                    if !me.project.is_empty() && me.project == g.project =>
                {
                    match g.access {
                        BrowserDbAccess::ReadWrite => "read_write",
                        BrowserDbAccess::ReadOnly => "read_only",
                    }
                }
                _ => "none",
            };
            let mut artifacts: Vec<String> = record
                .serve_entries()
                .into_iter()
                .map(|entry| entry.digest)
                .collect();
            artifacts.sort();
            artifacts.dedup();
            artifacts.truncate(MESH_ARTIFACTS_PER_PEER);
            MeshPeerView {
                endpoint_id: record.endpoint_id,
                scope: record.scope,
                project,
                db,
                artifacts,
            }
        })
        .collect();
    // Already ring-ordered by construction; sort so the wire order is the same
    // stable, id-ordered shape every other replicated set in this file uses.
    peers.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
    peers
}

fn mesh_capability_json(cloud: &Arc<CloudState>, me: MeshSelf<'_>, now: u64) -> Option<Value> {
    if !mesh_enabled() || me.endpoint_id.is_empty() {
        return None;
    }
    let peers = mesh_peers(cloud, me, now);
    // A donor alone in its tenant gets a block with an empty peer list rather
    // than no block: the two are different instructions to the worker
    // (`reconcileMeshLane` tears the lane DOWN on an absent block and on an
    // empty set alike, but only the block tells it the fleet HAS a signalling
    // arm). Sending it means the very next renewal, once a second tab joins,
    // starts the lane with no other state change.
    Some(json!({
        "enabled": true,
        "protocol_version": MESH_PROTOCOL_VERSION,
        "signal_path": "/v1/browser/signals",
        "signal_poll_ms": mesh_poll_ms(),
        "ice_servers": mesh_ice_servers(),
        "peers": peers
            .iter()
            .map(|peer| json!({
                "endpoint_id": peer.endpoint_id,
                "scope": match peer.scope {
                    BrowserScope::Public => "public",
                    BrowserScope::Team => "team",
                },
                "project": peer.project,
                "db": peer.db,
                "artifacts": peer.artifacts,
            }))
            .collect::<Vec<_>>(),
    }))
}

#[derive(Clone, Debug, Serialize)]
struct SignalMessage {
    seq: u64,
    from: String,
    kind: String,
    payload: String,
    sent_ms: u64,
}

#[derive(Default)]
struct SignalInbox {
    touched_ms: u64,
    messages: VecDeque<SignalMessage>,
}

#[derive(Default)]
struct SignalState {
    seq: u64,
    boxes: BTreeMap<String, SignalInbox>,
    delivered_total: u64,
    refused_total: u64,
    dropped_total: u64,
}

impl SignalState {
    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    /// Drop expired envelopes, then empty/idle inboxes, then — only if still
    /// over the ceiling — the least-recently-touched boxes. Runs on every call
    /// so the mailbox has no separate sweeper to fall behind.
    fn prune(&mut self, now: u64) {
        let floor = now.saturating_sub(SIGNAL_TTL_MS);
        for inbox in self.boxes.values_mut() {
            let before = inbox.messages.len();
            inbox.messages.retain(|message| message.sent_ms >= floor);
            self.dropped_total = self
                .dropped_total
                .saturating_add((before - inbox.messages.len()) as u64);
        }
        self.boxes
            .retain(|_, inbox| !inbox.messages.is_empty() || inbox.touched_ms >= floor);
        if self.boxes.len() <= SIGNAL_MAX_ENDPOINTS {
            return;
        }
        let mut by_age: Vec<(u64, String)> = self
            .boxes
            .iter()
            .map(|(id, inbox)| (inbox.touched_ms, id.clone()))
            .collect();
        by_age.sort();
        for (_, id) in by_age
            .into_iter()
            .take(self.boxes.len() - SIGNAL_MAX_ENDPOINTS)
        {
            self.boxes.remove(&id);
        }
    }
}

fn signal_state() -> &'static Mutex<SignalState> {
    static SIGNALS: OnceLock<Mutex<SignalState>> = OnceLock::new();
    SIGNALS.get_or_init(|| Mutex::new(SignalState::default()))
}

#[derive(Deserialize)]
struct SignalSend {
    to: String,
    kind: String,
    payload: String,
}

#[derive(Deserialize)]
struct SignalRequest {
    endpoint_id: String,
    /// Proof-of-possession, the SAME shape and the same verifier the admission
    /// POST uses — the platform session proves WHO the operator is, this
    /// proves the caller controls the endpoint key it claims to be signalling
    /// for. Without it a tenant member could drain another member's mailbox.
    #[serde(default)]
    challenge_ms: u64,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    protocol_version: u16,
    /// Highest inbox seq the caller has consumed. Everything at or below it is
    /// dropped from the caller's own box — the mailbox holds nothing a peer
    /// has already read.
    #[serde(default)]
    ack_seq: u64,
    #[serde(default)]
    send: Vec<SignalSend>,
}

/// One signalling round trip: deliver this caller's outbound envelopes, drain
/// its own inbox, in one authenticated POST.
///
/// Every refusal direction collapses to the same answer on purpose — unknown
/// endpoint, foreign tenant, expired admission and a `to` outside the caller's
/// server-derived peer set are indistinguishable, so this arm can never be
/// used to probe which browsers exist.
async fn browser_signals(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Json(request): Json<SignalRequest>,
) -> AdmissionResult {
    if !mesh_enabled() {
        return Err(AdmissionFailure::terminal(
            StatusCode::NOT_IMPLEMENTED,
            "mesh_disabled",
            "the browser peer mesh is disabled on this fleet",
        ));
    }
    if request.protocol_version != MESH_PROTOCOL_VERSION {
        return Err(AdmissionFailure::terminal(
            StatusCode::BAD_REQUEST,
            "signal_protocol_unsupported",
            "unsupported browser signalling protocol version",
        ));
    }
    let claims = claims_required(claims).map_err(|(_, message)| {
        AdmissionFailure::retryable(StatusCode::UNAUTHORIZED, "session_required", message)
    })?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    if !hive_browser_proto::valid_blake3_digest(&request.endpoint_id) {
        return Err(AdmissionFailure::terminal(
            StatusCode::BAD_REQUEST,
            "endpoint_id_invalid",
            "browser endpoint id is malformed",
        ));
    }
    verify_proof_of_possession(&request.endpoint_id, request.challenge_ms, &request.signature)?;
    let now = hive_core::now_ms();
    // The caller must STILL hold a live admission under the authenticated
    // tenant. This is re-read per call, not trusted from the session: a
    // revoked browser stops being able to signal within one round trip, on
    // exactly the same store the relay denylist and the gateway routes read.
    let Some(me) = cloud
        .browser_admissions
        .get(&tenant, &request.endpoint_id, now)
    else {
        return Err(AdmissionFailure::retryable(
            StatusCode::FORBIDDEN,
            "signal_not_admitted",
            "no live browser admission for this endpoint",
        ));
    };
    let authorized: BTreeSet<String> = mesh_peers(
        &cloud,
        MeshSelf {
            tenant: &tenant,
            endpoint_id: &me.endpoint_id,
            scope: me.scope,
            project: me
                .db
                .as_ref()
                .map(|grant| grant.project.as_str())
                .unwrap_or_default(),
        },
        now,
    )
    .into_iter()
    .map(|peer| peer.endpoint_id)
    .collect();

    let mut delivered = 0usize;
    let mut refused = 0usize;
    let mut state = signal_state().lock();
    state.prune(now);
    for entry in request.send.into_iter().take(SIGNAL_MAX_SEND_PER_CALL) {
        // A peer that legitimately left the set between the caller's last
        // capability and now is the COMMON case, not an attack — count and
        // skip rather than failing the whole call, which would also strand the
        // inbox drain below.
        if !authorized.contains(&entry.to)
            || !matches!(entry.kind.as_str(), "offer" | "answer" | "ice" | "bye")
            || entry.payload.len() > SIGNAL_MAX_PAYLOAD_BYTES
        {
            refused += 1;
            continue;
        }
        let seq = state.next_seq();
        let inbox = state.boxes.entry(entry.to).or_default();
        inbox.touched_ms = now;
        // Drop-OLDEST, never drop-newest: a full inbox means the peer is not
        // draining, and the newest envelope is the one its next poll can still
        // act on (an offer superseded by a re-offer, the latest ice batch).
        let evicted = if inbox.messages.len() >= SIGNAL_MAX_PER_ENDPOINT {
            inbox.messages.pop_front();
            1
        } else {
            0
        };
        inbox.messages.push_back(SignalMessage {
            seq,
            from: me.endpoint_id.clone(),
            kind: entry.kind,
            payload: entry.payload,
            sent_ms: now,
        });
        state.dropped_total = state.dropped_total.saturating_add(evicted);
        delivered += 1;
    }
    let inbox = state.boxes.entry(me.endpoint_id.clone()).or_default();
    inbox.touched_ms = now;
    // Drop what the caller has already consumed AND anything from a sender
    // that is no longer in its peer set. The authorization is checked on BOTH
    // ends of the mailbox, not just at send: a peer whose admission was
    // revoked between queueing an envelope and this poll has no business
    // reaching this browser, and dropping (rather than skipping) keeps a
    // now-unauthorized envelope from sitting at the head of the queue
    // occupying a delivery slot until its TTL. peer-mesh.js re-checks `from`
    // against its own capability as well — same rule, two independent halves.
    inbox
        .messages
        .retain(|message| message.seq > request.ack_seq && authorized.contains(&message.from));
    let messages: Vec<SignalMessage> = inbox
        .messages
        .iter()
        .take(SIGNAL_MAX_DELIVER_PER_CALL)
        .cloned()
        .collect();
    state.delivered_total = state.delivered_total.saturating_add(delivered as u64);
    state.refused_total = state.refused_total.saturating_add(refused as u64);
    let more = state.boxes[&me.endpoint_id].messages.len() > messages.len();
    drop(state);
    Ok(Json(json!({
        "protocol_version": MESH_PROTOCOL_VERSION,
        "messages": messages,
        "more": more,
        "delivered": delivered,
        "refused": refused,
        "retry_ms": mesh_poll_ms(),
    })))
}

/// Bounded, tenant-free mailbox counters for `/v1/browser/stats` — aggregates
/// only, same posture as every other counter in this file.
fn signal_stats() -> Value {
    let mut state = signal_state().lock();
    state.prune(hive_core::now_ms());
    json!({
        "enabled": mesh_enabled(),
        "inboxes": state.boxes.len(),
        "queued": state.boxes.values().map(|b| b.messages.len()).sum::<usize>(),
        "delivered_total": state.delivered_total,
        "refused_total": state.refused_total,
        "dropped_total": state.dropped_total,
    })
}

// ---------------------------------------------------------------------------
// The protocol-wide browser ROLL CALL.
//
// WHAT THIS IS. A periodic, read-only inventory + drift audit over the browser
// population: who is admitted, what each is authorized to serve, which
// database grant each holds, whether presence agrees, and whether this node's
// gateway routing layer matches the admissions that authorize it.
//
// WHY IT IS NOT A NEW PROTOCOL OP, AND NOT A CHANGE TO THE RENEWAL PATH.
// Both were considered and both are worse:
//
//   * A new `hive/browser/0` op ("are you there, what do you serve") would put
//     one outbound QUIC call per admitted browser on a timer, and could learn
//     NOTHING the platform is not already told: the admission renewal (a lease
//     tick of at most `MAX_LEASE_SECS`) already re-derives and re-states the
//     donor's whole serve set and db grant, and `browser_presence` already
//     replicates liveness. It would be a second, weaker source of truth for
//     facts that already replicate — and a source that a wedged tab answers
//     late or not at all, manufacturing "drift" out of its own timeouts.
//   * Extending the renewal path would make the sweep per-DONOR (fires when a
//     browser happens to renew, absent exactly when a browser stops renewing —
//     the case a roll call exists to catch) and would put audit work on a
//     latency-sensitive request path that every donor hits every 60s.
//
// So the roll call READS the state those two mechanisms already converge, on
// its own cadence, and reports what disagrees. It writes nothing: a sweep that
// repairs is a second writer to surfaces (`gw` routing, the admission store)
// that already have exactly one, and drift whose cause is unknown is not
// something an audit may resolve by mutating — the same discipline
// `browser_artifacts::gc` applies when its keep-set looks wrong.
//
// EVERY NODE RUNS IT, not the leader alone, because half of what it audits is
// node-local: the gateway browser target table is programmed per node from
// replicated admissions, so a route that leaked or never landed is a fact
// about THIS node and invisible from anywhere else. The inventory half reads
// the replicated admission/presence stores and is therefore identical on every
// converged node.
// ---------------------------------------------------------------------------

const DEFAULT_ROLL_CALL_SECS: u64 = 420; // 7 minutes
const MIN_ROLL_CALL_SECS: u64 = 30;
const ROLL_CALL_MAX_ROSTER: usize = 512;
const ROLL_CALL_MAX_DRIFT: usize = 128;
/// A browser admitted less than this ago has not necessarily published
/// presence yet (the worker posts it after boot), so a missing presence record
/// inside this window is a race, not drift. Flagging it would make every
/// normal start look like a fault.
const ROLL_CALL_PRESENCE_GRACE_MS: u64 = 90_000;

fn roll_call_interval_secs() -> u64 {
    match std::env::var("HIVE_BROWSER_ROLL_CALL_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => 0,
            Ok(secs) => secs.max(MIN_ROLL_CALL_SECS),
            Err(_) => DEFAULT_ROLL_CALL_SECS,
        },
        Err(_) => DEFAULT_ROLL_CALL_SECS,
    }
}

/// One disagreement found by a roll call. `kind` is a stable machine-readable
/// slug (operators alert on it); `detail` is the human half.
#[derive(Clone, Debug, Serialize)]
pub struct RollCallDrift {
    pub kind: &'static str,
    pub endpoint_id: String,
    pub tenant: String,
    pub detail: String,
}

/// One admitted browser as the roll call sees it.
#[derive(Clone, Debug, Serialize)]
pub struct RollCallEntry {
    pub endpoint_id: String,
    pub tenant: String,
    /// The constellation node name presence renders, when a presence record
    /// exists — empty when this browser is admitted but silent.
    pub node_name: String,
    pub presence: String,
    pub scope: &'static str,
    pub serves: Vec<BrowserServe>,
    pub db_project: String,
    pub db_access: Option<BrowserDbAccess>,
    /// May this peer hold fragments of GLOBAL platform state? Read from the
    /// PRESENCE record, which is where it is stamped (from the authenticated
    /// caller's `platform_admin` claim) — so an admitted-but-silent browser
    /// reports `false` here, which is also the safe reading: nothing plans
    /// against a peer with no live presence.
    pub shard_eligible: bool,
    /// How many browser peers this endpoint is authorized to address on the
    /// DIRECT lane right now (bn-browser-peer-webrtc-mesh) — the same
    /// server-derived set `capability.mesh` carries, counted. `0` means the
    /// direct lane has nothing to do for this browser (it is alone in its
    /// tenant, or the mesh is disabled fleet-wide), never that it failed.
    pub mesh_peers: usize,
    pub routes: usize,
    pub issued_ms: u64,
    pub expires_ms: u64,
    pub lease_remaining_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RollCall {
    pub started_ms: u64,
    pub took_ms: u64,
    pub node: String,
    pub leader: String,
    pub is_leader: bool,
    pub interval_secs: u64,
    pub admitted: usize,
    pub tenants: usize,
    pub presence_online: usize,
    pub presence_live: usize,
    /// Live presence records this node could not match to any live admission
    /// under the admitted tenants — a COUNT, not a list, because the presence
    /// store's cross-tenant view is deliberately not exposed (its per-tenant
    /// reader pins the tenant, and the roll call is not a reason to widen it).
    /// Non-zero means "a presence record outlived the admission that
    /// authorized it", the exact invariant `remove_for_endpoint` exists for.
    pub presence_orphans: usize,
    pub serve_entries: usize,
    pub db_grants: usize,
    /// Live presence records eligible to hold global state fragments — the
    /// shard planner's own membership bar, reported here so an operator can
    /// see it move without reading `browser_db::shard_plan`.
    pub shard_eligible: usize,
    pub routes: usize,
    pub roster_truncated: bool,
    pub drift_truncated: bool,
    pub drift: Vec<RollCallDrift>,
    pub roster: Vec<RollCallEntry>,
}

impl RollCall {
    /// The counters only — what `/v1/browser/stats` embeds, and what the log
    /// line carries. Never the roster: that names endpoints and tenants, and
    /// `browser_stats` is deliberately cardinality-free.
    fn summary(&self) -> Value {
        json!({
            "started_ms": self.started_ms,
            "took_ms": self.took_ms,
            "node": self.node,
            "interval_secs": self.interval_secs,
            "admitted": self.admitted,
            "tenants": self.tenants,
            "presence_online": self.presence_online,
            "presence_orphans": self.presence_orphans,
            "serve_entries": self.serve_entries,
            "db_grants": self.db_grants,
            "shard_eligible": self.shard_eligible,
            "routes": self.routes,
            "drift": self.drift.len(),
        })
    }
}

fn last_roll_call() -> &'static Mutex<Option<RollCall>> {
    static LAST: OnceLock<Mutex<Option<RollCall>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

/// Every Ready, browser-eligible (tenant, deployment, function) → the
/// descriptor's CURRENT canonical policy digest, built once per sweep.
///
/// `browser_artifacts::descriptor_for` answers the same question one entry at
/// a time by walking every deployment record; calling it per serve entry makes
/// the sweep quadratic in fleet size for no benefit, so the index is built
/// once from the identical two replicated sources.
fn browser_digest_index(cloud: &Arc<CloudState>) -> BTreeMap<(String, String, String), String> {
    let mut index = BTreeMap::new();
    for record in cloud.gw.deployment_records() {
        if record.state != fluid_core::DeployState::Ready {
            continue;
        }
        let tenant = crate::admin::record_tenant(&record.tenant).to_string();
        for function in &record.manifest.functions {
            if let Some(artifact) = &function.browser_artifact {
                index.insert(
                    (tenant.clone(), record.id.clone(), function.name.clone()),
                    artifact.policy_digest.clone(),
                );
            }
        }
    }
    for deployments in cloud.peer_deployments.read().values() {
        for info in deployments {
            if info.state != fluid_core::DeployState::Ready {
                continue;
            }
            let tenant = crate::admin::record_tenant(&info.tenant).to_string();
            for function in &info.browser_functions {
                index.insert(
                    (tenant.clone(), info.id.0.clone(), function.name.clone()),
                    function.artifact.policy_digest.clone(),
                );
            }
        }
    }
    index
}

/// Run one roll call. Pure read: it takes no lock it does not release and
/// mutates nothing outside the cached result.
pub fn roll_call(cloud: &Arc<CloudState>) -> RollCall {
    let started_ms = hive_core::now_ms();
    let admissions = cloud.browser_admissions.active(started_ms);
    let digests = browser_digest_index(cloud);
    let targets = cloud.gw.browser_targets();

    let mut routes_by_endpoint: BTreeMap<&str, Vec<&BrowserTarget>> = BTreeMap::new();
    for target in &targets {
        routes_by_endpoint
            .entry(target.endpoint_id.as_str())
            .or_default()
            .push(target);
    }
    let tenants: BTreeSet<&str> = admissions
        .iter()
        .map(|record| record.tenant.as_str())
        .collect();
    let mut presence: BTreeMap<String, crate::browser_presence::BrowserPresence> = BTreeMap::new();
    for tenant in &tenants {
        for record in cloud.browser_presence.list(tenant, started_ms) {
            presence.insert(record.endpoint_id.clone(), record);
        }
    }
    let presence_live: usize = cloud
        .browser_presence
        .stats()
        .by_state
        .values()
        .map(|count| *count as usize)
        .sum();

    // Direct-lane fan-out, counted from the tenant's population rather than by
    // re-deriving each browser's peer set (quadratic, for an answer that is the
    // same arithmetic every time). `mesh_degree` is the SAME formula the
    // capability derivation spends its budget with.
    let mut per_tenant: BTreeMap<&str, usize> = BTreeMap::new();
    for record in &admissions {
        *per_tenant.entry(record.tenant.as_str()).or_insert(0) += 1;
    }
    let peer_cap = mesh_max_peers();
    let mesh_on = mesh_enabled();

    let mut drift: Vec<RollCallDrift> = Vec::new();
    let mut roster: Vec<RollCallEntry> = Vec::new();
    let mut serve_entries = 0usize;
    let mut db_grants = 0usize;
    let mut presence_online = 0usize;
    let mut matched_presence = 0usize;
    let mut shard_eligible = 0usize;

    for record in &admissions {
        let entries = record.serve_entries();
        serve_entries += entries.len();
        let here = routes_by_endpoint
            .get(record.endpoint_id.as_str())
            .map(|targets| targets.as_slice())
            .unwrap_or(&[]);
        let live = presence.get(&record.endpoint_id);
        if live.is_some() {
            matched_presence += 1;
        }
        if live.is_some_and(|p| p.state == "online") {
            presence_online += 1;
        }
        if live.is_some_and(|p| p.shard_eligible) {
            shard_eligible += 1;
        }
        if live.is_none() && started_ms.saturating_sub(record.issued_ms) > ROLL_CALL_PRESENCE_GRACE_MS
        {
            drift.push(RollCallDrift {
                kind: "presence_missing",
                endpoint_id: record.endpoint_id.clone(),
                tenant: record.tenant.clone(),
                detail: format!(
                    "admitted {}s ago with no live presence record",
                    started_ms.saturating_sub(record.issued_ms) / 1_000
                ),
            });
        }
        for entry in &entries {
            let key = (
                record.tenant.clone(),
                entry.deployment.clone(),
                entry.function.clone(),
            );
            match digests.get(&key) {
                None => drift.push(RollCallDrift {
                    kind: "serve_descriptor_missing",
                    endpoint_id: record.endpoint_id.clone(),
                    tenant: record.tenant.clone(),
                    detail: format!(
                        "serves {}/{} but no Ready browser-eligible descriptor exists for it",
                        entry.deployment, entry.function
                    ),
                }),
                Some(current) if *current != entry.digest => drift.push(RollCallDrift {
                    kind: "serve_digest_stale",
                    endpoint_id: record.endpoint_id.clone(),
                    tenant: record.tenant.clone(),
                    detail: format!(
                        "serves {}/{} at {} while the descriptor is {}",
                        entry.deployment, entry.function, entry.digest, current
                    ),
                }),
                Some(_) => {}
            }
            // A serve entry with no matching gateway target means THIS node
            // never programmed (or has since lost) the route the replicated
            // admission authorizes — invocations land elsewhere or nowhere.
            let routed = here.iter().any(|target| {
                target.deployment == entry.deployment && target.function == entry.function
            });
            if !routed {
                drift.push(RollCallDrift {
                    kind: "route_missing",
                    endpoint_id: record.endpoint_id.clone(),
                    tenant: record.tenant.clone(),
                    detail: format!(
                        "authorized to serve {}/{} with no gateway route on this node",
                        entry.deployment, entry.function
                    ),
                });
            }
        }
        if let Some(grant) = &record.db {
            db_grants += 1;
            let expected_file = crate::browser_db::replica_file_name(&grant.project);
            if grant.db_file != expected_file {
                drift.push(RollCallDrift {
                    kind: "db_file_mismatch",
                    endpoint_id: record.endpoint_id.clone(),
                    tenant: record.tenant.clone(),
                    detail: format!(
                        "grant names replica {} but the platform template yields {}",
                        grant.db_file, expected_file
                    ),
                });
            }
            if crate::browser_db::db_descriptor_for(cloud, &record.tenant, &record.deployment)
                .is_none()
            {
                drift.push(RollCallDrift {
                    kind: "db_grant_orphaned",
                    endpoint_id: record.endpoint_id.clone(),
                    tenant: record.tenant.clone(),
                    detail: format!(
                        "holds a database grant for project {} whose deployment no longer carries a browser_db block",
                        grant.project
                    ),
                });
            }
        }
        roster.push(RollCallEntry {
            endpoint_id: record.endpoint_id.clone(),
            tenant: record.tenant.clone(),
            node_name: live.map(|p| p.node_name.clone()).unwrap_or_default(),
            presence: live
                .map(|p| p.state.clone())
                .unwrap_or_else(|| "absent".to_string()),
            scope: match record.scope {
                BrowserScope::Public => "public",
                BrowserScope::Team => "team",
            },
            serves: entries,
            db_project: record
                .db
                .as_ref()
                .map(|grant| grant.project.clone())
                .unwrap_or_default(),
            db_access: record.db.as_ref().map(|grant| grant.access),
            shard_eligible: live.is_some_and(|p| p.shard_eligible),
            mesh_peers: if mesh_on {
                mesh_degree(
                    per_tenant
                        .get(record.tenant.as_str())
                        .copied()
                        .unwrap_or(0),
                    peer_cap,
                )
            } else {
                0
            },
            routes: here.len(),
            issued_ms: record.issued_ms,
            expires_ms: record.expires_ms,
            lease_remaining_ms: record.expires_ms.saturating_sub(started_ms),
        });
    }

    // The other direction: a route this node holds that no live admission
    // authorizes. `reconcile`/`remove_endpoint` are supposed to make this
    // impossible, which is exactly why a non-zero count matters — it is a
    // browser still receiving invocations on a grant that has ended.
    let live_ids: BTreeSet<&str> = admissions
        .iter()
        .map(|record| record.endpoint_id.as_str())
        .collect();
    let by_endpoint: BTreeMap<&str, &BrowserAdmission> = admissions
        .iter()
        .map(|record| (record.endpoint_id.as_str(), record))
        .collect();
    for target in &targets {
        if !live_ids.contains(target.endpoint_id.as_str()) {
            drift.push(RollCallDrift {
                kind: "route_orphaned",
                endpoint_id: target.endpoint_id.clone(),
                tenant: target.tenant.clone(),
                detail: format!(
                    "gateway routes {}/{} for an endpoint with no live admission",
                    target.deployment, target.function
                ),
            });
            continue;
        }
        let Some(record) = by_endpoint.get(target.endpoint_id.as_str()) else {
            continue;
        };
        let entries = record.serve_entries();
        let authorized = entries
            .iter()
            .find(|entry| entry.deployment == target.deployment && entry.function == target.function);
        match authorized {
            None => drift.push(RollCallDrift {
                kind: "route_unauthorized",
                endpoint_id: target.endpoint_id.clone(),
                tenant: target.tenant.clone(),
                detail: format!(
                    "gateway routes {}/{} which this endpoint's admission does not authorize",
                    target.deployment, target.function
                ),
            }),
            Some(entry) if entry.digest != target.digest => drift.push(RollCallDrift {
                kind: "route_digest_stale",
                endpoint_id: target.endpoint_id.clone(),
                tenant: target.tenant.clone(),
                detail: format!(
                    "gateway routes {}/{} at {} while the admission authorizes {}",
                    target.deployment, target.function, target.digest, entry.digest
                ),
            }),
            Some(_) => {}
        }
    }

    let drift_truncated = drift.len() > ROLL_CALL_MAX_DRIFT;
    drift.truncate(ROLL_CALL_MAX_DRIFT);
    let roster_truncated = roster.len() > ROLL_CALL_MAX_ROSTER;
    roster.truncate(ROLL_CALL_MAX_ROSTER);

    RollCall {
        started_ms,
        took_ms: hive_core::now_ms().saturating_sub(started_ms),
        node: cloud.node_name.clone(),
        leader: cloud.control_plane_leader(),
        is_leader: cloud.is_control_plane_leader(),
        interval_secs: roll_call_interval_secs(),
        admitted: admissions.len(),
        tenants: tenants.len(),
        presence_online,
        presence_live,
        presence_orphans: presence_live.saturating_sub(matched_presence),
        serve_entries,
        db_grants,
        shard_eligible,
        routes: targets.len(),
        roster_truncated,
        drift_truncated,
        drift,
        roster,
    }
}

/// Run a roll call, cache it for `/v1/browser/rollcall`, and LOG it.
///
/// The log line is the half that works with no operator present: a clean sweep
/// is one INFO line (so the absence of roll calls is itself visible in the
/// journal), and any disagreement is a WARN naming the drift kinds and their
/// counts — a roll call nobody reads is not a roll call.
fn run_and_record(cloud: &Arc<CloudState>) -> RollCall {
    let result = roll_call(cloud);
    if result.drift.is_empty() && result.presence_orphans == 0 {
        tracing::info!(
            admitted = result.admitted,
            tenants = result.tenants,
            online = result.presence_online,
            serves = result.serve_entries,
            db_grants = result.db_grants,
            shard_eligible = result.shard_eligible,
            routes = result.routes,
            took_ms = result.took_ms,
            "browser roll call: clean"
        );
    } else {
        let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
        for entry in &result.drift {
            *kinds.entry(entry.kind).or_insert(0) += 1;
        }
        tracing::warn!(
            admitted = result.admitted,
            tenants = result.tenants,
            online = result.presence_online,
            serves = result.serve_entries,
            db_grants = result.db_grants,
            shard_eligible = result.shard_eligible,
            routes = result.routes,
            presence_orphans = result.presence_orphans,
            drift = result.drift.len(),
            truncated = result.drift_truncated,
            kinds = %kinds
                .iter()
                .map(|(kind, count)| format!("{kind}={count}"))
                .collect::<Vec<_>>()
                .join(","),
            took_ms = result.took_ms,
            "browser roll call: drift detected"
        );
    }
    *last_roll_call().lock() = Some(result.clone());
    result
}

/// The periodic sweep. Every node (see the section header), cadence from
/// `HIVE_BROWSER_ROLL_CALL_SECS` (default 420s = 7 minutes, floor 30s, `0`
/// disables). No jitter and no leader election: the pass is a read of
/// node-local + already-replicated state and contends for nothing, so a fleet
/// running it in the same second costs exactly as much as one running it
/// staggered.
pub fn spawn_roll_call(cloud: Arc<CloudState>) {
    let secs = roll_call_interval_secs();
    if secs == 0 {
        tracing::info!("browser roll call disabled (HIVE_BROWSER_ROLL_CALL_SECS=0)");
        return;
    }
    tokio::spawn(async move {
        // One early pass so a node that has just booted reports its browser
        // population without waiting a whole interval — the sweep is read-only,
        // so an early one costs nothing and answers "did this node come up with
        // the routes it should have".
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        loop {
            run_and_record(&cloud);
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        }
    });
}

#[derive(Deserialize)]
struct RollCallQuery {
    /// Recompute inline instead of serving the last cached sweep.
    ///
    /// This is what makes the endpoint honest behind round-robin DNS: the
    /// cached result is whatever THIS node's timer last produced, and a GET is
    /// served locally (AGENTS.md), so an operator hitting the public host
    /// otherwise reads an arbitrary node's arbitrary-age sweep. `?fresh=true`
    /// is still a pure read — it computes from replicated + node-local state
    /// and writes nothing but the cache.
    #[serde(default)]
    fresh: bool,
}

/// `GET /v1/browser/rollcall` (operator). Reports THIS node's view and says
/// so: the inventory half is replicated and identical everywhere once
/// converged, the routing half is per-node by construction. The dashboard's
/// `/ops/*` proxy forwards to the control-plane leader, so an operator going
/// through it always reads the same node's answer.
async fn roll_call_view(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Query(query): Query<RollCallQuery>,
) -> ApiResult {
    crate::admin::require_operator(claims.map(|c| c.0).as_ref())?;
    let result = if query.fresh {
        Some(run_and_record(&cloud))
    } else {
        last_roll_call().lock().clone()
    };
    match result {
        Some(result) => Ok(Json(json!({ "roll_call": result }))),
        // No sweep has run yet on this node (it booted seconds ago, or the
        // cadence is disabled). Never a fabricated empty result: an operator
        // must be able to tell "nothing admitted" from "never measured".
        None => Ok(Json(json!({
            "roll_call": Value::Null,
            "node": cloud.node_name,
            "interval_secs": roll_call_interval_secs(),
            "pending": true,
        }))),
    }
}

async fn list_admissions(State(cloud): State<Arc<CloudState>>, claims: Claims) -> ApiResult {
    let claims = claims_required(claims)?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let local = cloud.browser_admissions.list(&tenant, hive_core::now_ms());
    if local.is_empty() && !cloud.is_control_plane_leader() {
        let leader = cloud.control_plane_leader();
        if let Some(value) =
            crate::admin::fetch_from_host(&cloud, &leader, "/v1/browser/admissions", &tenant).await
        {
            return Ok(Json(value));
        }
    }
    Ok(Json(json!({ "admissions": local })))
}

async fn get_admission(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Path(endpoint_id): Path<String>,
) -> ApiResult {
    let claims = claims_required(claims)?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    if let Some(record) = cloud
        .browser_admissions
        .get(&tenant, &endpoint_id, hive_core::now_ms())
    {
        return Ok(Json(json!({ "admission": record })));
    }
    if !cloud.is_control_plane_leader() {
        let leader = cloud.control_plane_leader();
        let path = format!("/v1/browser/admissions/{endpoint_id}");
        if let Some(value) = crate::admin::fetch_from_host(&cloud, &leader, &path, &tenant).await {
            return Ok(Json(value));
        }
    }
    Err((StatusCode::NOT_FOUND, "browser admission not found".into()))
}

async fn accept_admission(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Path(endpoint_id): Path<String>,
) -> ApiResult {
    let claims = claims_required(claims)?;
    if !claims.platform_admin && !(claims.sub == "mesh-internal" && claims.role == "service") {
        return Err((
            StatusCode::FORBIDDEN,
            "browser admission acceptance is mesh-internal".into(),
        ));
    }
    Ok(Json(json!({
        "admitted": cloud
            .browser_admissions
            .endpoint_active(&endpoint_id, hive_core::now_ms())
    })))
}

async fn revoke_admission(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Path(endpoint_id): Path<String>,
) -> ApiResult {
    let claims = claims_required(claims)?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let current = cloud
        .browser_admissions
        .get(&tenant, &endpoint_id, hive_core::now_ms())
        .ok_or((StatusCode::NOT_FOUND, "browser admission not found".into()))?;
    if current.subject != claims.sub && !matches!(claims.role.as_str(), "owner" | "admin") {
        return Err((
            StatusCode::FORBIDDEN,
            "cannot revoke another user's browser session".into(),
        ));
    }
    cloud.browser_admissions.revoke(&tenant, &endpoint_id);
    fanout_revoke(&cloud, &endpoint_id);
    remove_endpoint(&cloud, &endpoint_id).await;
    Ok(Json(json!({ "ok": true, "revoked": endpoint_id })))
}

/// Fan the fast-path revoke echo to every OTHER healthy peer reachable over
/// the mesh (bn-p2p-revocation-latency's remaining item) -- shrinks the
/// window a revoked endpoint can still route through / reconnect to a
/// follower's relay from the ~60s periodic store_sync snapshot-pull interval
/// down to one mesh round trip. Best-effort and backgrounded: the caller
/// (revoke_admission/revoke_team) already applied the authoritative change
/// locally and is about to return to ITS caller, so a peer this misses
/// (partition, no gossiped iroh address yet) simply catches up on the next
/// periodic pull -- this is a latency optimization layered on top of an
/// already-correct mechanism, never a replacement for it.
fn fanout_revoke(cloud: &Arc<CloudState>, endpoint_id: &str) {
    let targets: Vec<(String, String)> = cloud
        .registry
        .nodes()
        .iter()
        .filter(|n| n.healthy && n.name != cloud.node_name)
        .filter_map(|n| Some((n.peer_id.clone()?, n.iroh_addr.clone()?)))
        .collect();
    if targets.is_empty() {
        return;
    }
    let cloud = cloud.clone();
    let endpoint_id = endpoint_id.to_string();
    tokio::spawn(async move {
        let path = format!("/v1/browser/admissions/mesh-revoke/{endpoint_id}");
        for (id, addr) in targets {
            let ok =
                crate::gossip::request_to(&cloud, &id, &addr, hive_p2p::GOSSIP_POST, &path, &[], 5)
                    .await
                    .is_some();
            tracing::debug!(endpoint_id = %endpoint_id, node = %id, ok, "browser admission revoke echo");
        }
    });
}

/// Receiving side of `fanout_revoke`'s echo, dispatched via
/// `gossip::dispatch`'s `/v1/browser/admissions/mesh-revoke/:endpoint_id`
/// POST arm. See `BrowserAdmissionStore::mark_denied` for why this touches
/// ONLY the relay denylist + gateway routing, never the versioned
/// active/tombstone state.
pub async fn mesh_revoke_echo(cloud: &Arc<CloudState>, endpoint_id: &str) {
    cloud.browser_admissions.mark_denied(endpoint_id, hive_core::now_ms());
    remove_endpoint(cloud, endpoint_id).await;
}

/// Fan the fresh-admission deny-CLEAR echo to every OTHER healthy peer
/// (bn-relay-denylist-restart-friction) -- the exact mirror of
/// [`fanout_revoke`]: the leader's `put` already cleared its own denylist
/// entry for a re-admitted endpoint, and this shrinks the window followers
/// keep denying its relay reconnection from the ~60s periodic store_sync
/// snapshot-pull interval down to one mesh round trip. Same best-effort
/// contract: a missed peer simply converges on the next pull.
fn fanout_deny_clear(cloud: &Arc<CloudState>, endpoint_id: &str) {
    let targets: Vec<(String, String)> = cloud
        .registry
        .nodes()
        .iter()
        .filter(|n| n.healthy && n.name != cloud.node_name)
        .filter_map(|n| Some((n.peer_id.clone()?, n.iroh_addr.clone()?)))
        .collect();
    if targets.is_empty() {
        return;
    }
    let cloud = cloud.clone();
    let endpoint_id = endpoint_id.to_string();
    tokio::spawn(async move {
        let path = format!("/v1/browser/admissions/mesh-deny-clear/{endpoint_id}");
        for (id, addr) in targets {
            let ok =
                crate::gossip::request_to(&cloud, &id, &addr, hive_p2p::GOSSIP_POST, &path, &[], 5)
                    .await
                    .is_some();
            tracing::debug!(endpoint_id = %endpoint_id, node = %id, ok, "browser admission deny-clear echo");
        }
    });
}

/// Receiving side of `fanout_deny_clear`'s echo, dispatched via
/// `gossip::dispatch`'s `/v1/browser/admissions/mesh-deny-clear/:endpoint_id`
/// POST arm. Denylist only (see `BrowserAdmissionStore::mark_deny_cleared`)
/// — deliberately does NOT touch gateway routing or presence: the leader's
/// admission snapshot programs those, and a follower removing routes on this
/// echo could tear down a route its own (newer) snapshot already restored.
pub async fn mesh_deny_clear_echo(cloud: &Arc<CloudState>, endpoint_id: &str) {
    cloud
        .browser_admissions
        .mark_deny_cleared(endpoint_id, hive_core::now_ms());
}

/// Did this renewal INVALIDATE anything the previous lease could route?
///
/// The teardown this gates (drop the gateway route, drop presence, close the
/// BrowserPool trunk) exists so an invocation cannot straddle two identities —
/// reuse an open trunk with a grant that has since moved. A pure ADDITION to
/// the serve set (the tenant deployed something new, which in auto mode happens
/// on an ordinary renewal) invalidates nothing: every previously routable entry
/// is still routable, byte for byte. Treating it as an identity change would
/// close the browser's trunk and blank its constellation presence every time
/// anyone in the tenant deployed.
///
/// So: the address/tenant/scope/database-GRANT moving is still an identity
/// change, and so is any previous serve entry that is GONE or ROTATED — but a
/// superset is not.
///
/// The database half compares the GRANT, not the `deployment` id it resolved
/// through: redeploying the opted-in project mints a new deployment id while
/// the grant (project, replica file, access, caps) is byte-identical, and that
/// is not a change of identity — it is the same browser replicating the same
/// database. The scalar `deployment` field is otherwise only the pre-`serves`
/// compat mirror of a serve entry, which the set test below already covers.
fn routing_identity_changed(old: &BrowserAdmission, new: &BrowserAdmission) -> bool {
    if old.addr_json != new.addr_json
        || old.tenant != new.tenant
        || old.scope != new.scope
        || old.db != new.db
    {
        return true;
    }
    let after = new.serve_entries();
    !old.serve_entries()
        .iter()
        .all(|before| after.contains(before))
}

async fn close_endpoint(cloud: &Arc<CloudState>, endpoint_id: &str) {
    let pool = { cloud.browser_mesh.read().clone() };
    if let Some(pool) = pool {
        pool.close_endpoint(endpoint_id).await;
    }
}

async fn remove_endpoint(cloud: &Arc<CloudState>, endpoint_id: &str) {
    cloud.gw.remove_browser_endpoint(endpoint_id);
    // A presence record must never outlive the admission that authorized it.
    crate::browser_presence::remove_for_endpoint(cloud, endpoint_id);
    close_endpoint(cloud, endpoint_id).await;
}

/// Read-only accessor for other browser-lifecycle modules (presence) that
/// need to confirm the caller owns a live admission without reaching into
/// `CloudState` directly.
pub(crate) fn local_admission(
    cloud: &Arc<CloudState>,
    tenant: &str,
    endpoint_id: &str,
    now: u64,
) -> Option<BrowserAdmission> {
    cloud.browser_admissions.get(tenant, endpoint_id, now)
}

pub async fn revoke_team(cloud: &Arc<CloudState>, tenant: &str) -> usize {
    let tenant = crate::admin::norm(tenant).to_string();
    let removed = cloud.browser_admissions.revoke_team(&tenant);
    for record in &removed {
        fanout_revoke(cloud, &record.endpoint_id);
        remove_endpoint(cloud, &record.endpoint_id).await;
    }
    removed.len()
}

pub fn snapshot_bytes(cloud: &Arc<CloudState>) -> Vec<u8> {
    if cloud.is_control_plane_leader() {
        let expired = cloud.browser_admissions.expire(hive_core::now_ms());
        for record in expired {
            cloud.gw.remove_browser_endpoint(&record.endpoint_id);
            // Same invariant `remove_endpoint` states for the REVOKE path — "a
            // presence record must never outlive the admission that authorized
            // it" — applied to the EXPIRY path, which was missing it. Without
            // this an admission that simply aged out left its presence record
            // behind, so the constellation kept drawing a satellite for a
            // browser that no longer had any right to serve, until presence's
            // own TTL happened to catch up.
            crate::browser_presence::remove_for_endpoint(cloud, &record.endpoint_id);
            let cloud = cloud.clone();
            tokio::spawn(async move {
                close_endpoint(&cloud, &record.endpoint_id).await;
            });
        }
    }
    serde_json::to_vec(&cloud.browser_admissions.snapshot()).unwrap_or_default()
}

pub fn adopt_snapshot(cloud: &Arc<CloudState>, bytes: &[u8]) -> Option<usize> {
    let incoming: BrowserAdmissionSnapshot = serde_json::from_slice(bytes).ok()?;
    let (old, new) = cloud.browser_admissions.adopt(incoming)?;
    reconcile(cloud, &old, &new);
    Some(new.active.len())
}

fn reconcile(
    cloud: &Arc<CloudState>,
    old: &BrowserAdmissionSnapshot,
    new: &BrowserAdmissionSnapshot,
) {
    let ids: BTreeSet<String> = old
        .active
        .keys()
        .chain(new.active.keys())
        .cloned()
        .collect();
    let now = hive_core::now_ms();
    for id in ids {
        let before = old.active.get(&id);
        let after = new.active.get(&id).filter(|record| record.expires_ms > now);
        if before == after {
            continue;
        }
        match after {
            Some(record) => {
                if before.is_some_and(|before| routing_identity_changed(before, record)) {
                    cloud.gw.remove_browser_endpoint(&id);
                    schedule_close(cloud, id.clone());
                }
                if !record.serving() {
                    // Target-less / database-only donor
                    // (browser-node-optional-serve-target): a live admission
                    // with deliberately NO serve route. Removing rather than
                    // skipping keeps this idempotent against any earlier
                    // serving lease for the same endpoint id.
                    cloud.gw.remove_browser_endpoint(&id);
                    continue;
                }
                // The whole authorized set, replaced atomically — a follower
                // programs exactly what the leader authorized, never a merge of
                // this snapshot with what it happened to hold before. A
                // pre-`serves` record replicated by an older leader resolves to
                // its scalar entry (see `serve_entries`).
                if let Err(error) = cloud.gw.set_browser_targets(&id, record.targets()) {
                    tracing::warn!(endpoint_id = %id, %error, "rejected replicated browser admission");
                }
            }
            None => {
                cloud.gw.remove_browser_endpoint(&id);
                schedule_close(cloud, id.clone());
            }
        }
    }
}

fn schedule_close(cloud: &Arc<CloudState>, endpoint_id: String) {
    let cloud = cloud.clone();
    tokio::spawn(async move {
        close_endpoint(&cloud, &endpoint_id).await;
    });
}

pub async fn endpoint_admitted(cloud: &Arc<CloudState>, endpoint_id: &str) -> bool {
    if cloud
        .browser_admissions
        .endpoint_active(endpoint_id, hive_core::now_ms())
    {
        return true;
    }
    if cloud.is_control_plane_leader() {
        return false;
    }
    // A hostile peer can generate unlimited valid endpoint identities. Bound
    // miss fallbacks separately so random-id connection floods cannot amplify
    // into an unbounded request storm against the control-plane leader.
    static FALLBACKS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    let limit = std::env::var("HIVE_BROWSER_ADMISSION_FALLBACKS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(8);
    let semaphore = FALLBACKS.get_or_init(|| tokio::sync::Semaphore::new(limit));
    let Ok(_permit) = semaphore.try_acquire() else {
        tracing::warn!(endpoint_id, "browser admission leader fallback saturated");
        return false;
    };
    let leader = cloud.control_plane_leader();
    let path = format!("/v1/browser/admissions/accept/{endpoint_id}");
    crate::admin::fetch_from_host(cloud, &leader, &path, "")
        .await
        .and_then(|value| value.get("admitted").and_then(Value::as_bool))
        .unwrap_or(false)
}

pub fn mesh_accept(cloud: &Arc<CloudState>, endpoint_id: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "admitted": cloud
            .browser_admissions
            .endpoint_active(endpoint_id, hive_core::now_ms())
    }))
    .unwrap_or_default()
}

pub fn mesh_list(cloud: &Arc<CloudState>, tenant: &str) -> Vec<u8> {
    let records = cloud
        .browser_admissions
        .list(&crate::admin::norm(tenant), hive_core::now_ms());
    serde_json::to_vec(&json!({ "admissions": records })).unwrap_or_default()
}

pub fn mesh_get(cloud: &Arc<CloudState>, tenant: &str, endpoint_id: &str) -> Vec<u8> {
    let record = cloud.browser_admissions.get(
        &crate::admin::norm(tenant),
        endpoint_id,
        hive_core::now_ms(),
    );
    record
        .map(|record| serde_json::to_vec(&json!({ "admission": record })).unwrap_or_default())
        .unwrap_or_default()
}
