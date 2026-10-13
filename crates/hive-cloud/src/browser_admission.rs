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
use std::sync::{Arc, Once, OnceLock};

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
/// How long an explicit revoke also denies the endpoint at the RELAY layer: far shorter than
/// `TOMBSTONE_RETENTION_MS` and never permanent, because a browser that reloads keeps its persisted
/// identity and would otherwise be denied forever.
const RELAY_DENYLIST_RETENTION_MS: u64 = 10 * 60 * 1_000;

/// One (deployment, function, digest) triple a browser may serve. Every field is SERVER-DERIVED at
/// admit/renewal from replicated deployment state under the authenticated tenant; even an explicit
/// pin takes its digest from the build-stamped descriptor, never the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserServe {
    pub deployment: String,
    pub function: String,
    /// The descriptor's canonical policy digest — THE wire digest
    /// (`encode_invoke`, the artifact URL, the donor's pin).
    pub digest: String,
}

impl BrowserServe {
    /// A serve entry is routable only with all three identifiers;
    /// `fluid_gateway::set_browser_targets` re-checks independently.
    fn complete(&self) -> bool {
        !self.deployment.is_empty() && !self.function.is_empty() && !self.digest.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserAdmission {
    pub endpoint_id: String,
    pub addr_json: String,
    /// The deployment this browser's DATABASE grant is pinned to (and, for pre-`serves` peers, its
    /// one visible serve entry), or EMPTY: a plain `String` so a pre-upgrade follower parses the
    /// snapshot unchanged. INVARIANT: a non-empty `function`/`digest` names a function OF THIS
    /// deployment, a coherent member of [`Self::serve_entries`], never a cross-deployment mixture
    /// (that could run B's digest under A's route).
    #[serde(default)]
    pub deployment: String,
    /// The function of `deployment` this browser serves, or EMPTY when it contributes no serve lane
    /// (database-only, or no target).
    #[serde(default)]
    pub function: String,
    /// The scalar entry's canonical policy digest, or EMPTY without a scalar serve lane;
    /// [`Self::serving`] is the ONE predicate callers use.
    #[serde(default)]
    pub digest: String,
    /// The COMPLETE set of (deployment, function, digest) triples this browser may serve: every
    /// browser-eligible function of every Ready deployment its tenant owns, re-derived on each
    /// renewal (or exactly one entry for an explicit pin). `serde(default)` for rollout: a record
    /// from a pre-upgrade leader has no set, and [`Self::serve_entries`] falls back to the scalar
    /// triple: one entry, never zero or a wildcard.
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
    /// The server-derived browser-DATABASE grant, present only when the admitted deployment's
    /// descriptor carries a `browser_db` block (and, for Public scope, `public_read: true`); the
    /// grant `browser_db::resolve_round_grant` re-checks. `serde(default)`: absent means NO db
    /// capability, never a wrong one.
    #[serde(default)]
    pub db: Option<BrowserDbGrant>,
}

/// The replicated half of a database grant: server-derived and tenant-pinned, riding the admission
/// and dying with its lease; no field comes from donor input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDbGrant {
    /// The project the database belongs to (database identity IS the
    /// project; the grant is resolved against this exact deployment).
    pub project: String,
    pub access: BrowserDbAccess,
    /// Caps resolved from the spec at issue time (the exchange re-resolves the LIVE spec per
    /// request); the donor reconciles its own enforcement from this snapshot.
    pub max_bytes: u64,
    pub max_value_bytes: u64,
    /// Platform-templated replica name (`browser_db::replica_file_name`): the browser opens exactly
    /// this OPFS file.
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
    /// The routable serve entries this record authorizes: the ONE resolver every routing decision
    /// funnels through. Prefers the replicated set, falls back to the scalar triple for a
    /// pre-`serves` leader's record, and drops incomplete entries (the gateway refuses them too).
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
    /// EXPLICIT revocations only (endpoint_id -> revoked-at ms): unlike `tombstones`, a lease
    /// `expire()` never writes here, or every browser whose lease merely ran out would be denied
    /// reconnection at the relay.
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
        self.denylist
            .retain(|_, revoked_at| *revoked_at >= deny_floor);
    }
}

/// Bounded, tenant-free counters: global aggregates only, never a per-tenant or per-endpoint
/// breakdown, so exposing them cannot leak cardinality or identify a peer.
#[derive(Default, Serialize)]
pub struct BrowserAdmissionCounters {
    pub admissions_total: u64,
    pub renewals_total: u64,
    pub revocations_total: u64,
    pub expirations_total: u64,
    pub denials_total: u64,
    /// Live size of the relay-level denylist (self-prunes on `RELAY_DENYLIST_RETENTION_MS`).
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

    /// Every live admission across EVERY tenant, ordered by endpoint id: the roll call's inventory
    /// view and nothing else (operator-only, read-only); every tenant-facing reader goes through
    /// [`Self::list`]/[`Self::get`], which pin the tenant.
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

    /// The LIVE record for an endpoint regardless of tenant, for the CRR exchange's re-check
    /// (`browser_db::resolve_round_grant`); the endpoint id is the QUIC-authenticated identity, and
    /// the caller re-checks tenant pinning against the record's own fields.
    pub fn live_for_endpoint(&self, endpoint_id: &str, now: u64) -> Option<BrowserAdmission> {
        self.inner
            .lock()
            .active
            .get(endpoint_id)
            .filter(|record| record.expires_ms > now)
            .cloned()
    }

    /// Explicit-revocation check for the relay's `AccessControl`, deliberately NOT the negation of
    /// `endpoint_active`: a browser between leases is not denied, only one an operator actually
    /// revoked.
    pub fn is_denied(&self, endpoint_id: &str, now: u64) -> bool {
        let mut state = self.inner.lock();
        state.prune_tombstones(now);
        state.denylist.contains_key(endpoint_id)
    }

    /// Stores an admitted record (refused while another active session owns the endpoint). A fresh,
    /// fully validated admission (session + proof-of-possession + server-resolved descriptor)
    /// SUPERSEDES the endpoint's relay-denylist entry, since a stop-then-start is a deliberate
    /// restart, not revocation evasion; a bare revoke keeps denying for its whole retention window.
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

    /// Fast-path relay deny for a follower echoing a leader's revoke
    /// (`fanout_revoke`/`mesh_revoke_echo`): the denylist only, never `revoke()`'s versioned
    /// active/tombstone mutation, since bumping this node's wall-clock version would advertise a
    /// logical time it never earned (pre-upgrade followers gate their wholesale replace on it).
    /// `adopt` re-derives the entry's fate on every merge.
    fn mark_denied(&self, endpoint_id: &str, now: u64) {
        let mut state = self.inner.lock();
        state.denylist.insert(endpoint_id.to_string(), now);
        state.prune_tombstones(now);
    }

    /// The exact inverse of [`mark_denied`] for a follower echoing a leader's fresh admission
    /// (`fanout_deny_clear`): denylist only, so a stop-then-start need not wait for the next
    /// store_sync snapshot adoption to be let back onto this follower's relay.
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
        state
            .denylist
            .insert(endpoint_id.to_string(), hive_core::now_ms());
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

    /// Merges a snapshot PER ENDPOINT ID, never wholesale (a wall-clock-versioned replace silently
    /// destroyed admissions made through the losing node): the higher `revision` wins (ties keep
    /// local); a tombstone at or above a record's revision removes it while a strictly newer
    /// re-admission survives; tombstones union by max revision; the relay denylist unions by max
    /// revoked-at, minus entries a newer admission supersedes. `version` is only `max`ed, never
    /// re-stamped (pre-upgrade followers gate their wholesale replace on it). A record dropped
    /// without a tombstone may be resurrected by a peer, bounded by its lease and every reader's
    /// `expires_ms` filter. `None` means nothing changed, so `adopt_snapshot` skips `reconcile`.
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

        state.version = state.version.max(incoming.version);
        state.prune_tombstones(hive_core::now_ms());

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
    /// OPTIONAL serve target, decided server-side in `validate_request`: `deployment` + `function`
    /// pins exactly that function; `deployment` alone is database-only (it must resolve under the
    /// tenant and carry a `browser_db` block: no artifact, route or invoker grant); neither is the
    /// automatic shape per `serve_mode`. A `function` without a `deployment` is rejected; naming a
    /// deployment always overrides `serve_mode`.
    #[serde(default)]
    deployment: String,
    #[serde(default)]
    function: String,
    /// What a donor that named NO deployment wants: `"auto"` serves every browser-eligible function
    /// its tenant owns, re-derived on each renewal. It is a REQUEST, never a capability: the server
    /// resolves the set itself from replicated state under the authenticated tenant. Anything else,
    /// INCLUDING ABSENT, is capacity only (no artifact, route or grant): a pre-upgrade worker sends
    /// none, and a rollout must never start serving code on a donor that did not ask.
    #[serde(default)]
    serve_mode: String,
    /// ROLLOUT-COMPATIBILITY ONLY: accepted but NEVER consulted. The effective digest is resolved
    /// server-side from the build-stamped descriptor, so a forged, stale or missing donor digest
    /// all yield the descriptor's canonical policy digest; the field stays so a pre-change worker
    /// keeps deserializing.
    #[serde(default)]
    #[allow(dead_code)]
    digest: String,
    #[serde(default)]
    lease_secs: Option<u64>,
    #[serde(default)]
    scope: BrowserScope,
    protocol_version: u16,
    /// Proof-of-possession: the caller's own current-time claim (ms since epoch), signed together
    /// with `endpoint_id` (see `signature`). `serde(default)` so an older worker is rejected by
    /// `validate_request`'s freshness/signature check rather than by a generic-looking 400
    /// deserialize failure.
    #[serde(default)]
    challenge_ms: u64,
    /// 128 hex chars: the ed25519 signature (`hive_browser::BrowserNode::signAdmission`) over
    /// `"{endpoint_id}:{challenge_ms}"`, proving the caller controls the private key of
    /// `endpoint_id`; platform auth alone would let anyone name ANY endpoint id.
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
        .route("/v1/browser/signals", post(browser_signals))
        .route("/v1/browser/deployments/:id/status", get(deployment_status))
}

/// Tenant-scoped "is my browser function actually being served right now": ONE call joining the
/// deployment's descriptors, the live admissions pinned to them, those endpoints' presence, the
/// artifact host set and this node's locally verified byte copy. A foreign tenant and an unknown id
/// answer the identical 404 (no existence leak).
async fn deployment_status(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Path(id): Path<String>,
) -> ApiResult {
    let claims = claims_required(claims)?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let now = hive_core::now_ms();

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
        let live: Vec<_> = admissions
            .iter()
            .filter(|a| {
                a.serve_entries()
                    .iter()
                    .any(|entry| entry.deployment == id && entry.digest == descriptor.policy_digest)
            })
            .collect();
        let online = presence
            .iter()
            .filter(|p| p.state == "online" && live.iter().any(|a| a.endpoint_id == p.endpoint_id))
            .count();
        let artifact_hosts = crate::browser_artifacts::resolve_for_tenant(
            &cloud,
            &tenant,
            &descriptor.policy_digest,
        )
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

/// Bounded, tenant-free operational counters: global aggregates across BOTH browser stores plus the
/// BrowserPool's dial/invoke/byte counters, never a per-tenant or per-endpoint breakdown. `pool` is
/// `null` before the first browser-capable iroh endpoint binds and `roll_call` (counters only; the
/// roster that names endpoints lives behind `/v1/browser/rollcall`) is `null` before this node's
/// first sweep, never fabricated zeros.
async fn browser_stats(State(cloud): State<Arc<CloudState>>, claims: Claims) -> ApiResult {
    crate::admin::require_operator(claims.map(|c| c.0).as_ref())?;
    let pool_stats = cloud.browser_mesh.read().as_ref().map(|pool| pool.stats());
    Ok(Json(json!({
        "admissions": cloud.browser_admissions.stats(),
        "presence": cloud.browser_presence.stats(),
        "pool": pool_stats,
        "signals": signal_stats(),
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

/// Divisions (Clerk org tenants) whose MEMBERS, not just owners/admins, may run a PUBLIC browser
/// node: `HIVE_PUBLIC_NODE_TENANTS` (comma-separated, normalized) overrides the default
/// `thoth-division`. Such a member mints `role: "member"`, which the base owner/admin gate would
/// reject.
fn public_node_tenants() -> Vec<String> {
    let raw =
        std::env::var("HIVE_PUBLIC_NODE_TENANTS").unwrap_or_else(|_| "thoth-division".to_string());
    raw.split(',')
        .map(|s| crate::admin::norm(s).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// May this caller run a PUBLIC-scope browser node (which exposes a deployment to ANY anonymous
/// donor)? Yes for a `platform_admin`, for a tenant-scoped `owner`/`admin`, or for a member of a
/// public-node division (see `public_node_tenants`), keyed on the Clerk-verified `tenant` claim so
/// it cannot be spoofed.
fn may_serve_public(claims: &crate::auth::Claims) -> bool {
    if claims.platform_admin || matches!(claims.role.as_str(), "owner" | "admin") {
        return true;
    }
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    public_node_tenants().iter().any(|t| *t == tenant)
}

/// Proof-of-possession: rejects an admission whose `challenge_ms` is stale (this freshness window
/// IS the replay defense; there is no nonce round trip) or whose signature does not verify against
/// `endpoint_id`'s own key (the endpoint id string IS the hex ed25519 public key).
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

/// One authorized serve entry plus the metadata the capability block carries; `project` is display
/// context, the authorization is the (deployment, function) -> descriptor resolution.
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
    /// The deployment the DATABASE grant resolves against, or empty: explicit when the donor named
    /// one, the tenant's single opted-in project in auto mode, never an arbitrary pick among
    /// several (see `browser_db::auto_db_deployment_for_tenant`).
    db_deployment: String,
    /// True when the set was derived automatically rather than pinned.
    auto: bool,
}

/// Authorizes an admission request, deriving everything server-side under the AUTHENTICATED tenant:
/// the donor's `digest` is never consulted; a named function must resolve to a Ready deployment
/// whose build stamped a browser artifact descriptor; with no function there is no artifact
/// capability at all; a database-only shape must still resolve its deployment under this tenant
/// (unknown deployment, foreign tenant and removed block answer identically); with no deployment
/// and `serve_mode: "auto"` the whole eligible set is derived. The
/// `protocol_too_old:`/`protocol_too_new:` message prefixes are a wire contract the worker
/// pattern-matches.
fn validate_request(
    cloud: &Arc<CloudState>,
    claims: &crate::auth::Claims,
    request: &AdmissionRequest,
) -> Result<Authorized, AdmissionFailure> {
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
                project: project_of(cloud, &tenant, deployment),
                function: descriptor.name,
                artifact: descriptor.artifact,
            }],
        }
    } else if auto {
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

/// The project label of a deployment under `tenant`, display context only (never an authorization
/// input); empty when the replicated view does not name it.
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
    let denied_before_put = cloud
        .browser_admissions
        .is_denied(&request.endpoint_id, issued_ms);
    let serve_entries: Vec<BrowserServe> = serves
        .iter()
        .map(|grant| BrowserServe {
            deployment: grant.deployment.clone(),
            function: grant.function.clone(),
            digest: grant.artifact.policy_digest.clone(),
        })
        .collect();
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
        db: db_capability.as_ref().map(|(grant, _)| grant.clone()),
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
        remove_endpoint(&cloud, &record.endpoint_id).await;
    }
    if record.serving() {
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
    if denied_before_put {
        fanout_deny_clear(&cloud, &record.endpoint_id);
    }
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

/// The server-derived capability `admit` returns (initial and renewal): ONE atomic snapshot the
/// donor reconciles from. `artifacts[]` is the COMPLETE authorized descriptor set (digests verbatim
/// from the build-stamped descriptor; `deployment`/`function`/`project` are display-only, never
/// authorization inputs); `trusted_callers` is [`trusted_caller_ids`]; `db`, `mesh` and `net` ride
/// the same snapshot. An EMPTY set carries `serving: false`, no artifact fields and no callers
/// (absence of capability, never a broader one). ROLLOUT: the FIRST entry is also mirrored into the
/// flat `artifact_url`/`policy_digest`/... fields a pre-`artifacts[]` worker reads.
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
    if let Some((tenant, grant, resolved)) = db {
        out["db"] =
            crate::browser_db::capability_db_json(cloud, tenant, grant, resolved, expires_ms);
    }
    if let Some(mesh) = mesh_capability_json(cloud, me, hive_core::now_ms()) {
        out["mesh"] = mesh;
    }
    out["net"] = wisp_net_json();
    out
}

/// The third-party public fallback relay (upstream node-worker's README example): never a default,
/// reachable only through `HIVE_BROWSER_WISP_PUBLIC_FALLBACK`, named in the capability's
/// `disclosure`, and used only after the donor proves it answers a websocket handshake.
const PUBLIC_WISP_FALLBACK_URL: &str = "wss://anura.pro/";

/// The platform relay (`HIVE_BROWSER_WISP_URL`, an operator-run relay: the only source that keeps
/// donor traffic off third-party infrastructure), or `None`. `ws://` is accepted but WARNed (donor
/// traffic in the clear); anything that is not a `ws:`/`wss:` URL yields NO relay, never a
/// half-parsed one.
fn wisp_relay_url() -> Option<String> {
    let raw = std::env::var("HIVE_BROWSER_WISP_URL").unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if !is_wisp_url(raw) {
        tracing::warn!(
            "HIVE_BROWSER_WISP_URL is not a ws:// or wss:// URL — browser node:net / node:tls stay unconfigured"
        );
        return None;
    }
    if raw.starts_with("ws://") {
        tracing::warn!(
            "HIVE_BROWSER_WISP_URL is plaintext ws:// — donor-browser outbound TCP/TLS metadata travels unencrypted to the relay"
        );
    }
    Some(raw.to_string())
}

/// Scheme + a host, and nothing that could smuggle a second origin in: the
/// donor dials this string as given.
fn is_wisp_url(raw: &str) -> bool {
    if raw.chars().any(char::is_whitespace) {
        return false;
    }
    let rest = raw.strip_prefix("wss://").or_else(|| raw.strip_prefix("ws://"));
    let Some(rest) = rest else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host_and_port = authority.rsplit('@').next().unwrap_or_default();
    let host = match host_and_port.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
        Some(_) => return false,
        None => host_and_port,
    };
    !host.is_empty() && !host.contains('?') && !host.contains('#')
}

/// Is the third-party public fallback permitted at all? OFF unless the operator turns it on.
fn wisp_public_fallback() -> bool {
    matches!(
        std::env::var("HIVE_BROWSER_WISP_PUBLIC_FALLBACK").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    )
}

/// The capability's `net` block, all operator configuration (none of it from the request):
/// `wisp_url` is the platform relay, else only with the explicit public-fallback opt-in the
/// third-party relay; with neither it is `null` and the donor's `node:net`/`node:tls` throw a NAMED
/// error at first use, never failing silently. Logged once per process.
fn wisp_net_json() -> Value {
    static LOGGED: Once = Once::new();
    let relay = wisp_relay_url();
    let fallback = wisp_public_fallback();
    LOGGED.call_once(|| match (&relay, fallback) {
        (Some(url), false) => tracing::info!(
            relay = %wisp_log_label(url),
            "browser node:net / node:tls configured over the platform wisp relay"
        ),
        (Some(url), true) => tracing::info!(
            relay = %wisp_log_label(url),
            fallback = PUBLIC_WISP_FALLBACK_URL,
            "browser node:net / node:tls configured over the platform wisp relay (public fallback armed)"
        ),
        (None, true) => tracing::warn!(
            "browser wisp PUBLIC FALLBACK in use: donor-browser outbound TCP/TLS transits the \
             third-party relay {} — every connection is terminated and re-emitted there, so that \
             relay sees donor traffic this platform does not control. Set HIVE_BROWSER_WISP_URL \
             to an operator-run relay and HIVE_BROWSER_WISP_PUBLIC_FALLBACK=0 to stop it.",
            PUBLIC_WISP_FALLBACK_URL
        ),
        (None, false) => tracing::info!(
            "no HIVE_BROWSER_WISP_URL and no public fallback — browser node:net / node:tls are \
             unavailable in donors (they throw a named error at first use, they do not fail silently)"
        ),
    });
    json!({
        "wisp_url": relay,
        "allow_public_fallback": fallback,
        "public_fallback_url": PUBLIC_WISP_FALLBACK_URL,
        "disclosure": match (&relay, fallback) {
            (Some(url), false) => format!(
                "outbound TCP/TLS from this browser transits the operator-configured wisp relay {}",
                wisp_log_label(url)
            ),
            (Some(url), true) => format!(
                "outbound TCP/TLS from this browser transits the operator-configured wisp relay \
                 {}, or the third-party public relay {} when that one is unreachable",
                wisp_log_label(url),
                PUBLIC_WISP_FALLBACK_URL
            ),
            (None, true) => format!(
                "outbound TCP/TLS from this browser transits the THIRD-PARTY public wisp relay \
                 {} — that relay terminates and re-emits every connection",
                PUBLIC_WISP_FALLBACK_URL
            ),
            (None, false) => "no wisp relay is configured — node:net and node:tls are unavailable \
                              in this browser"
                .to_string(),
        },
    })
}

/// Scheme + host only: a wisp v1 relay carries its token in the PATH, so logging the URL verbatim
/// would put a credential in the journal.
fn wisp_log_label(url: &str) -> String {
    let scheme = url.split("://").next().unwrap_or("wss");
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default();
    format!("{scheme}://{host}/")
}

/// The fleet EndpointIds allowed to originate BrowserPool invokes against an admitted browser:
/// every HEALTHY node in the live registry, by proven iroh identity (from the gossiped
/// `EndpointAddr`, else the join-verified `peer_id`). Never client input, node NAMES, browser ids
/// or a wildcard (`grantInvoker` takes one exact EndpointId); renewal re-derives the set.
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

/// Default per-donor peer-set ceiling: each peer costs the browser an RTCPeerConnection +
/// DataChannel, a real per-tab bound (peer-mesh.js independently caps at 32).
const MESH_MAX_PEERS: usize = 16;
const MESH_PEER_HARD_CAP: usize = 32;
const MESH_ARTIFACTS_PER_PEER: usize = 64;
const MESH_PROTOCOL_VERSION: u16 = 1;

/// Mailbox bounds. Every one of them is enforced per CALL as well as per
/// endpoint, so a donor cannot convert the signalling arm into storage.
const SIGNAL_TTL_MS: u64 = 60_000;
/// A full inbox drops its OLDEST envelope: a peer that is not draining can still act on the newest
/// (a re-offer, the latest ice batch).
const SIGNAL_MAX_PER_ENDPOINT: usize = 32;
const SIGNAL_MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const SIGNAL_MAX_SEND_PER_CALL: usize = 16;
const SIGNAL_MAX_DELIVER_PER_CALL: usize = 16;
/// Hard ceiling on inboxes held at once; the least-recently-touched box is evicted, which costs its
/// owner one handshake retry and never an admission.
const SIGNAL_MAX_ENDPOINTS: usize = 4096;

/// Is the direct lane offered at all? `HIVE_BROWSER_MESH=0` removes the capability block and makes
/// the signalling arm answer 501, the shape peer-mesh.js already treats as "no signalling arm yet":
/// the kill switch and the pre-upgrade rollout state are one donor code path.
fn mesh_enabled() -> bool {
    !matches!(
        std::env::var("HIVE_BROWSER_MESH").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// The per-donor degree ceiling, floored at 2: the topology is a symmetric ring (`cap/2` successors
/// + `cap/2` predecessors), which a budget of 1 cannot split.
fn mesh_max_peers() -> usize {
    std::env::var("HIVE_BROWSER_MESH_MAX_PEERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= 2)
        .unwrap_or(MESH_MAX_PEERS)
        .min(MESH_PEER_HARD_CAP)
}

/// Peers ONE member of an `n`-browser tenant may address under the degree ceiling; the capability
/// derivation and the roll call share this ONE formula so they cannot drift.
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

/// ICE servers offered to donors: none unless `HIVE_BROWSER_ICE_SERVERS` (a JSON array of
/// RTCIceServer objects, the only form that can carry TURN credentials, or a plain comma-separated
/// URL list) is set. There is no default endpoint (it would put a third party on every donor's
/// connection path) and the lane degrades to the relay. A MALFORMED value yields NO servers, never
/// a partial list; peer-mesh.js re-validates.
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

/// The requesting donor's identity for the mesh derivation: a struct because every field is an
/// authorization input and swapping two is a capability bug.
#[derive(Clone, Copy)]
struct MeshSelf<'a> {
    tenant: &'a str,
    endpoint_id: &'a str,
    scope: BrowserScope,
    /// The database project THIS donor holds a grant for, or empty: a browser holds ONE replica, so
    /// a peer replicating another project has no database relationship with it.
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

/// The browser endpoints `me` may address, re-derived from LIVE admissions on every call
/// (capability issue and each signalling round trip): same tenant, self excluded, none when `me` is
/// not in the list (the ring has no origin). The rule is SYMMETRIC by construction: below the
/// degree ceiling everyone addresses everyone; above it the sorted ids form a RING and each member
/// takes `cap/2` successors and `cap/2` predecessors, so "B in A's set" and "A in B's set" are one
/// statement (a plain "first N" is not: one side would offer into a mailbox its owner must drop,
/// forever). Database access is `"none"` unless both sides are Team scope on the same project.
fn mesh_peers(cloud: &Arc<CloudState>, me: MeshSelf<'_>, now: u64) -> Vec<MeshPeerView> {
    let team_scoped_self = me.scope == BrowserScope::Team;
    let mut records = cloud.browser_admissions.list(me.tenant, now);
    records.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
    let n = records.len();
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
    peers.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
    peers
}

/// A donor alone in its tenant still gets the block, with an empty peer list: only the block tells
/// the worker the fleet HAS a signalling arm, so the lane starts on the next renewal once a second
/// tab joins.
fn mesh_capability_json(cloud: &Arc<CloudState>, me: MeshSelf<'_>, now: u64) -> Option<Value> {
    if !mesh_enabled() || me.endpoint_id.is_empty() {
        return None;
    }
    let peers = mesh_peers(cloud, me, now);
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

    /// Drops expired envelopes, then empty/idle inboxes, then (only if still over the ceiling) the
    /// least-recently-touched boxes; runs on every call, so there is no sweeper to fall behind.
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
    /// Proof-of-possession, the same shape and verifier as the admission POST: without it a tenant
    /// member could drain another member's mailbox.
    #[serde(default)]
    challenge_ms: u64,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    protocol_version: u16,
    /// Highest inbox seq the caller has consumed; everything at or below it is dropped from the
    /// caller's own box.
    #[serde(default)]
    ack_seq: u64,
    #[serde(default)]
    send: Vec<SignalSend>,
}

/// One signalling round trip: delivers the caller's envelopes and drains its inbox in one
/// authenticated POST (a POST because the mailbox is node-local and GETs are served by whichever
/// node DNS picks). The caller's live admission is re-read every call, `to` must be in the
/// server-derived peer set, and every refusal is the SAME answer, so the arm cannot probe which
/// browsers exist. The mailbox moves opaque JSON: the sender signs its DTLS fingerprint with its
/// ed25519 endpoint key and the receiver verifies it against the endpoint id THIS server admitted,
/// so a compromised mailbox can drop a handshake but never sit in the middle of one.
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
    verify_proof_of_possession(
        &request.endpoint_id,
        request.challenge_ms,
        &request.signature,
    )?;
    let now = hive_core::now_ms();
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

const DEFAULT_ROLL_CALL_SECS: u64 = 7 * 60;
const MIN_ROLL_CALL_SECS: u64 = 30;
/// A freshly booted node reports its browser population without waiting a whole interval (the
/// sweep is read-only, so an early pass is free and shows whether the node came up with its routes).
const FIRST_ROLL_CALL_DELAY_SECS: u64 = 20;
const ROLL_CALL_MAX_ROSTER: usize = 512;
const ROLL_CALL_MAX_DRIFT: usize = 128;
/// A browser admitted less than this ago may not have published presence yet (the worker posts it
/// after boot), so a missing presence record inside the window is a race, not drift.
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
    /// May this peer hold fragments of GLOBAL platform state? Read from the PRESENCE record
    /// (stamped from the caller's `platform_admin` claim), so an admitted-but-silent browser
    /// reports `false`, the safe reading.
    pub shard_eligible: bool,
    /// How many browser peers this endpoint may address on the DIRECT lane right now (the
    /// `capability.mesh` set, counted); `0` means nothing to do (alone in its tenant, or the mesh
    /// is disabled), never failure.
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
    /// Live presence records matching no live admission under the admitted tenants: a COUNT, not a
    /// list (the presence store's cross-tenant view is deliberately not exposed). Non-zero means a
    /// presence record outlived the admission that authorized it, the invariant
    /// `remove_for_endpoint` exists for.
    pub presence_orphans: usize,
    pub serve_entries: usize,
    pub db_grants: usize,
    /// Live presence records eligible to hold global state fragments: the shard planner's own
    /// membership bar, reported so an operator can see it move.
    pub shard_eligible: usize,
    pub routes: usize,
    pub roster_truncated: bool,
    pub drift_truncated: bool,
    pub drift: Vec<RollCallDrift>,
    pub roster: Vec<RollCallEntry>,
}

impl RollCall {
    /// The counters only (what `/v1/browser/stats` embeds and the log line carries), never the
    /// roster, which names endpoints and tenants.
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

/// Every Ready, browser-eligible (tenant, deployment, function) -> its CURRENT canonical policy
/// digest, built once per sweep from the same two sources as `browser_artifacts::descriptor_for`
/// (which walks every deployment per entry and would make the sweep quadratic).
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
        if live.is_none()
            && started_ms.saturating_sub(record.issued_ms) > ROLL_CALL_PRESENCE_GRACE_MS
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
                    per_tenant.get(record.tenant.as_str()).copied().unwrap_or(0),
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
        let authorized = entries.iter().find(|entry| {
            entry.deployment == target.deployment && entry.function == target.function
        });
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
        leader: cloud.control_plane_leader().unwrap_or_default(),
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

/// Runs a roll call, caches it for `/v1/browser/rollcall` and LOGS it: a clean sweep is one INFO
/// line (so missing roll calls are visible), any disagreement a WARN naming the drift kinds and
/// counts.
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

/// The periodic roll call: a read-only inventory and drift audit of the browser population
/// (admissions, serve sets, db grants, presence, and this node's gateway routes against the
/// admissions that authorize them). EVERY node runs it (the gateway target table is node-local); it
/// writes nothing, since a repairing sweep would be a second writer. `HIVE_BROWSER_ROLL_CALL_SECS`:
/// default 7 min, floor 30 s, `0` disables.
pub fn spawn_roll_call(cloud: Arc<CloudState>) {
    let secs = roll_call_interval_secs();
    if secs == 0 {
        tracing::info!("browser roll call disabled (HIVE_BROWSER_ROLL_CALL_SECS=0)");
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(FIRST_ROLL_CALL_DELAY_SECS)).await;
        loop {
            run_and_record(&cloud);
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        }
    });
}

#[derive(Deserialize)]
struct RollCallQuery {
    /// Recompute inline instead of serving the last cached sweep: behind round-robin DNS a GET is
    /// served locally, so the cache is whatever THIS node's timer last produced. `?fresh=true` is
    /// still a pure read (it writes only the cache).
    #[serde(default)]
    fresh: bool,
}

/// `GET /v1/browser/rollcall` (operator): THIS node's view, and it says so: the inventory half is
/// replicated and identical once converged, the routing half is per-node. Before the node's first
/// sweep it answers `roll_call: null` with `pending: true`, never a fabricated empty result. The
/// dashboard's `/ops/*` proxy forwards to the leader, so it always reads one node.
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
    if let (true, Some(leader)) = (local.is_empty(), cloud.leader_forward_target()) {
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
    if let Some(leader) = cloud.leader_forward_target() {
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

/// Fans the fast-path revoke echo to every OTHER healthy peer over the mesh, shrinking the window a
/// revoked endpoint can still reach a follower's relay from the ~60 s store_sync pull to one round
/// trip. Best-effort and backgrounded: the authoritative change is already applied locally, and a
/// missed peer catches up on the next pull.
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

/// Receiving side of `fanout_revoke` (`gossip::dispatch` POST
/// `/v1/browser/admissions/mesh-revoke/:endpoint_id`): ONLY the relay denylist + gateway routing,
/// never the versioned active/tombstone state (see `mark_denied`).
pub async fn mesh_revoke_echo(cloud: &Arc<CloudState>, endpoint_id: &str) {
    cloud
        .browser_admissions
        .mark_denied(endpoint_id, hive_core::now_ms());
    remove_endpoint(cloud, endpoint_id).await;
}

/// The mirror of [`fanout_revoke`] for a fresh admission: tells every OTHER healthy peer to clear
/// its relay-denylist entry, so a stop-then-start is not denied on a follower for the ~60 s
/// snapshot-pull interval; same best-effort contract.
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

/// Receiving side of `fanout_deny_clear` (`gossip::dispatch` POST
/// `/v1/browser/admissions/mesh-deny-clear/:endpoint_id`): the denylist only, never routing or
/// presence, which the leader's snapshot programs (a follower could tear down a route its own newer
/// snapshot already restored).
pub async fn mesh_deny_clear_echo(cloud: &Arc<CloudState>, endpoint_id: &str) {
    cloud
        .browser_admissions
        .mark_deny_cleared(endpoint_id, hive_core::now_ms());
}

/// Did this renewal INVALIDATE anything the previous lease could route? The teardown it gates
/// (route, presence, BrowserPool trunk) exists so an invocation cannot straddle two identities; a
/// pure ADDITION to the serve set invalidates nothing, so it must not tear down the trunk each time
/// the tenant deploys. A moved address, tenant, scope or database GRANT (compared by project,
/// replica file, access and caps, not the deployment id it resolved through) is a change, as is any
/// previous serve entry GONE or ROTATED.
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
    crate::browser_presence::remove_for_endpoint(cloud, endpoint_id);
    close_endpoint(cloud, endpoint_id).await;
}

/// Read-only accessor for other browser-lifecycle modules (presence) to confirm the caller owns a
/// live admission without reaching into `CloudState`.
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
    if crate::leadership::may_act(cloud, crate::leadership::Job::BrowserExpiry) {
        let expired = cloud.browser_admissions.expire(hive_core::now_ms());
        for record in expired {
            cloud.gw.remove_browser_endpoint(&record.endpoint_id);
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

/// Programs this node's gateway from an adopted snapshot: each endpoint's authorized set is
/// REPLACED atomically (never merged with what a follower already held), and a serve-less
/// (database-only) admission REMOVES any route instead of skipping, so an earlier serving lease
/// cannot linger.
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
                    cloud.gw.remove_browser_endpoint(&id);
                    continue;
                }
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
    /// Bounds miss fallbacks to the control-plane leader: a hostile peer can mint unlimited valid
    /// endpoint identities, and random-id connection floods must not amplify into an unbounded
    /// request storm against it.
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
    let Some(leader) = cloud.leader_forward_target() else {
        return false;
    };
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
