//! Fresh-session admission for low-trust browser serving peers.
//!
//! Browser identities never enter the fleet registry or trusted peer set. The
//! control-plane leader owns this short-lived store; followers adopt versioned
//! snapshots and only use the records to program Gateway's browser target layer.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use fluid_gateway::{BrowserScope, BrowserTarget};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

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
