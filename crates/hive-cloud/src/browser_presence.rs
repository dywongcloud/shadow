//! Replicated, TTL-bound coarse presence for admitted browser peers.
//!
//! Separate from `browser_admission` (which gates whether a browser endpoint
//! may serve at all) and from `NodeInfo`/the fleet registry — a presence
//! record exists only alongside a live admission for the same endpoint, is
//! never read by placement/scheduling/DNS/health, and carries no exact
//! location: coordinates are quantized server-side regardless of what a
//! client claims, because a client-only quantization step is trivially
//! bypassed by a direct API call.
//!
//! Two identity facts on every record are derived SERVER-SIDE and are never
//! client input, the same discipline as `browser_admission`'s capabilities:
//!
//! * [`node_name`] — the browser peer's own map/legend identity, a pure
//!   function of its PROVEN endpoint id (the QUIC public key the admission
//!   was minted against). Stable across reconnects of the same identity,
//!   short enough to render under a cube, and prefixed `bn-` so it can never
//!   be mistaken for a fleet node name.
//! * `shard_eligible` — whether this peer may hold fragments of GLOBAL
//!   platform state (`browser_db`'s shard planner). Stamped from the
//!   authenticated caller's `platform_admin` claim at upsert; a browser can
//!   no more assert it than it can assert its own admission.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;
type ApiResult = Result<Json<Value>, (StatusCode, String)>;

/// Coarse grid step in degrees. ~55km at the equator, tighter near the poles —
/// enough to place a satellite dot on a world map without revealing a home
/// address. Applied server-side unconditionally; a client's own precision
/// claim is never trusted past this floor.
const GEO_QUANT_DEGREES: f64 = 0.5;
const MIN_ACCURACY_KM: f64 = 25.0;
const MAX_ACCURACY_KM: f64 = 20_000.0;
const PRESENCE_TTL_SECS: u64 = 90;
const TOMBSTONE_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;
const VALID_STATES: [&str; 4] = ["starting", "online", "degraded", "suspended"];

/// Domain separator for [`node_name`]'s digest. Prefixing the endpoint id
/// keeps the naming hash from ever colliding with any other digest the
/// platform computes over the same id.
const NODE_NAME_DOMAIN: &str = "hive-browser-node-name-v1\u{0}";

/// The `bn-` prefix is load-bearing, not decoration: fleet node names are
/// operator-chosen (`fc-virginia`, `sj2`, `node-b`) and a browser peer's name
/// must be unmistakable on the same diagram, in the same legend, and in the
/// same log line. Nothing else in the platform mints a name in this shape.
const NODE_NAME_PREFIX: &str = "bn-";

/// 32 × 32 = 1024 mnemonic pairs, × 65536 tags = ~67M display names. Kept
/// short (≤7 chars each) so `bn-<adj>-<noun>-<tag>` still fits under a cube
/// on the constellation. Both lists are FIXED: changing an entry renames
/// every browser that hashed to it, which breaks exactly the "stable across
/// reconnects" property this exists for. Append only.
const NAME_ADJECTIVES: [&str; 32] = [
    "amber", "azure", "brisk", "calm", "clear", "coral", "crisp", "dusk", "ember", "fern", "flint",
    "frost", "gold", "ivory", "jade", "lilac", "lunar", "mint", "noble", "ochre", "olive", "onyx",
    "opal", "pearl", "plum", "quiet", "rapid", "rust", "sage", "slate", "solar", "teal",
];
const NAME_NOUNS: [&str; 32] = [
    "adder", "badger", "bison", "cedar", "crane", "dove", "eagle", "falcon", "finch", "gull",
    "hawk", "heron", "ibis", "koi", "lark", "lynx", "marlin", "moth", "otter", "owl", "panda",
    "pike", "quail", "raven", "robin", "seal", "shrew", "stork", "swift", "tern", "vole", "wren",
];

/// The stable, human-readable map identity of a browser peer, derived only
/// from its PROVEN endpoint id (`bn-teal-otter-9c1f`).
///
/// Properties this must keep, in the order they matter:
///
/// * **Server-derived.** The endpoint id is the QUIC public key the admission
///   was minted against, so the name is a function of proven identity — a
///   browser cannot pick, spoof or squat one, and there is no field on the
///   wire for it to try.
/// * **Stable across reconnects.** Same identity in, same name out, forever;
///   there is no clock, no counter and no membership input.
/// * **Distinguishable from a fleet node.** [`NODE_NAME_PREFIX`] plus the
///   fixed three-part shape.
/// * **Short.** ≤ 22 characters, renderable under a 9px cube.
///
/// It is a DISPLAY name, never an identity key: two endpoint ids can in
/// principle land on the same mnemonic (birthday-bounded at ~67M), so every
/// record still carries `endpoint_id` and every lookup still keys on it. The
/// tag is taken from the endpoint id's own leading hex where possible — an
/// operator reading `bn-teal-otter-9c1f` off the map can eyeball-match it
/// against `9c1f…` in the admissions table — and padded from the digest only
/// for an id too short or too exotic to supply four.
/// Subject prefix the fleet-hosted headless browser nodes mint under
/// (`scripts/hive-browser-node-broker.mjs`: `fleet-browser-node:<node>`).
const FLEET_BROWSER_NODE_SUBJECT_PREFIX: &str = "fleet-browser-node:";

/// Whether this session belongs to one of the FLEET'S OWN headless browser
/// nodes, which are shard-eligible without being `platform_admin`.
///
/// Why this is not a privilege hole: the subject is not client-assertable. That
/// token is minted only through `admin::mint_token`, which requires
/// `x-hive-internal == HIVE_INTERNAL_TOKEN` (constant-time compare, FAILS CLOSED
/// when the token is unconfigured), the broker reads that secret from a
/// root-only credential file, and the dashboard proxy strips a client-supplied
/// `x-hive-internal`. So this subject can only be produced by a process already
/// running as root on a fleet host — an operator, by definition. Anyone able to
/// forge it already holds the fleet's internal credential and needs no help from
/// this function.
///
/// Why it is NEEDED: shard eligibility was `claims.platform_admin` alone, and the
/// broker deliberately mints least-privilege (role=member, platform_admin=false —
/// a hostile mint body asking for `platform_admin:true` is refused). The five
/// operator-run browser nodes were therefore permanently excluded from
/// `shard_members`, leaving membership to be whatever human admin tabs happened
/// to be open. Witnessed on the leader: membership oscillating members=2 -> 1 ->
/// 0 -> 1 with `under_replicated=65` of 65 fragments, and at members=0,
/// `unplaced=65 refusals=65` — i.e. dedicated always-on infrastructure existed
/// and could hold none of it, which is the opposite of "always replicate".
fn is_fleet_browser_node(subject: &str) -> bool {
    subject
        .strip_prefix(FLEET_BROWSER_NODE_SUBJECT_PREFIX)
        .is_some_and(|node| !node.is_empty())
}

pub fn node_name(endpoint_id: &str) -> String {
    let digest = fluid_core::browser_source_digest(&format!("{NODE_NAME_DOMAIN}{endpoint_id}"));
    let nibble = |range: std::ops::Range<usize>| -> usize {
        usize::from_str_radix(digest.get(range).unwrap_or("0"), 16).unwrap_or(0)
    };
    let adjective = NAME_ADJECTIVES[nibble(0..4) % NAME_ADJECTIVES.len()];
    let noun = NAME_NOUNS[nibble(4..8) % NAME_NOUNS.len()];
    let mut tag: String = endpoint_id
        .chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect();
    if tag.len() < 4 {
        tag.extend(digest.chars().take(4 - tag.len()));
    }
    format!("{NODE_NAME_PREFIX}{adjective}-{noun}-{tag}")
}

/// Repair a record on the way OUT of the store: fill `node_name` when it is
/// empty (a record written by a pre-`node_name` leader, or one adopted from
/// such a peer). Applied on every read path — HTTP list, single get, mesh
/// list, shard membership — so no consumer ever has to special-case an
/// anonymous browser peer.
///
/// This is a recomputation, not a fallback: the name is a pure function of
/// `endpoint_id`, so the value written here is exactly the value the issuing
/// node would have written. `shard_eligible` is deliberately NOT repaired —
/// it encodes an authorization decision made against a live session, and
/// there is nothing on the record to re-derive it from.
fn hydrate(mut record: BrowserPresence) -> BrowserPresence {
    if record.node_name.is_empty() {
        record.node_name = node_name(&record.endpoint_id);
    }
    record
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrowserPresence {
    pub endpoint_id: String,
    pub tenant: String,
    pub subject: String,
    /// Tenant-scoped display token — never the raw platform identity.
    pub display_label: String,
    /// The peer's own node identity on the constellation ([`node_name`]).
    ///
    /// `serde(default)` for the rollout, and EMPTY is repaired rather than
    /// tolerated: [`hydrate`] recomputes it on every read, so a record
    /// replicated by a pre-upgrade leader still renders a name. Recomputing
    /// is not a guess — the name is a pure function of `endpoint_id`, which
    /// the record already carries, so the repaired value is bit-identical to
    /// what the issuing node would have written.
    #[serde(default)]
    pub node_name: String,
    /// May this peer hold fragments of GLOBAL platform state
    /// (`browser_db::shard_plan`)? Stamped from the authenticated caller's
    /// `platform_admin` claim at upsert — never from the request body.
    ///
    /// `false` is the safe default in every direction: a pre-upgrade record
    /// carries no field and is therefore ineligible (absent capability, never
    /// wrong capability), and a non-admin tenant's browser is ineligible by
    /// construction. See `browser_db`'s shard-trust block for why the bar is
    /// platform-admin today and what would have to exist to lower it.
    #[serde(default)]
    pub shard_eligible: bool,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub accuracy_km: Option<f64>,
    /// When the underlying location fix was captured, so a stale-but-not-yet
    /// expired record can still show its own age honestly.
    #[serde(default)]
    pub located_ms: Option<u64>,
    pub relay_hint: String,
    pub state: String,
    pub issued_ms: u64,
    pub expires_ms: u64,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PresenceSnapshot {
    version: u64,
    active: BTreeMap<String, BrowserPresence>,
    tombstones: BTreeMap<String, u64>,
}

impl PresenceSnapshot {
    fn new() -> Self {
        Self {
            version: hive_core::now_ms().max(1),
            active: BTreeMap::new(),
            tombstones: BTreeMap::new(),
        }
    }

    fn next_version(&mut self) -> u64 {
        self.version = hive_core::now_ms().max(self.version.saturating_add(1));
        self.version
    }

    fn prune_tombstones(&mut self, now: u64) {
        let floor = now.saturating_sub(TOMBSTONE_RETENTION_MS);
        self.tombstones.retain(|_, revision| *revision >= floor);
    }
}

/// Bounded, tenant-free counters (bn-p2p-observability) — global aggregates
/// only, same posture as browser_admission.rs's counters.
#[derive(Default, Serialize)]
pub struct BrowserPresenceCounters {
    pub upserts_total: u64,
    pub clears_total: u64,
    pub expirations_total: u64,
    /// Live gauge (not cumulative): count of currently-active, non-expired
    /// records grouped by `state` ("starting"/"online"/"degraded"/
    /// "suspended") — a fleet-wide aggregate only, never per-tenant, so it
    /// answers "how many browser peers are online right now" without naming
    /// any of them.
    pub by_state: BTreeMap<String, u64>,
}

#[derive(Default)]
struct PresenceCounterCells {
    upserts_total: std::sync::atomic::AtomicU64,
    clears_total: std::sync::atomic::AtomicU64,
    expirations_total: std::sync::atomic::AtomicU64,
}

pub struct BrowserPresenceStore {
    inner: Mutex<PresenceSnapshot>,
    counters: PresenceCounterCells,
}

impl BrowserPresenceStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(PresenceSnapshot::new()),
            counters: PresenceCounterCells::default(),
        }
    }

    pub fn stats(&self) -> BrowserPresenceCounters {
        use std::sync::atomic::Ordering::Relaxed;
        let now = hive_core::now_ms();
        let mut by_state: BTreeMap<String, u64> = BTreeMap::new();
        for record in self.inner.lock().active.values() {
            if record.expires_ms > now {
                *by_state.entry(record.state.clone()).or_insert(0) += 1;
            }
        }
        BrowserPresenceCounters {
            upserts_total: self.counters.upserts_total.load(Relaxed),
            clears_total: self.counters.clears_total.load(Relaxed),
            expirations_total: self.counters.expirations_total.load(Relaxed),
            by_state,
        }
    }

    pub(crate) fn list(&self, tenant: &str, now: u64) -> Vec<BrowserPresence> {
        self.inner
            .lock()
            .active
            .values()
            .filter(|record| record.tenant == tenant && record.expires_ms > now)
            .cloned()
            .map(hydrate)
            .collect()
    }

    /// Every live record eligible to hold GLOBAL state fragments, ACROSS
    /// tenants — the shard planner's membership set
    /// (`browser_db::shard_members`).
    ///
    /// Deliberately not tenant-filtered: platform state is not a tenant's
    /// data, so its shard membership is fleet-wide. What keeps that sound is
    /// the `shard_eligible` gate, which only a `platform_admin` session can
    /// set, plus the caller's own live-admission re-check. `starting`,
    /// `degraded` and `suspended` are all excluded — a peer that is not
    /// serving right now must not be planned against, or every transient
    /// blip reshuffles the map. Sorted by endpoint id so every node derives
    /// the identical membership list from the identical replicated state.
    pub(crate) fn shard_candidates(&self, now: u64) -> Vec<BrowserPresence> {
        let mut out: Vec<BrowserPresence> = self
            .inner
            .lock()
            .active
            .values()
            .filter(|record| {
                record.shard_eligible && record.state == "online" && record.expires_ms > now
            })
            .cloned()
            .map(hydrate)
            .collect();
        out.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
        out
    }

    fn get(&self, tenant: &str, endpoint_id: &str, now: u64) -> Option<BrowserPresence> {
        self.inner
            .lock()
            .active
            .get(endpoint_id)
            .filter(|record| record.tenant == tenant && record.expires_ms > now)
            .cloned()
            .map(hydrate)
    }

    fn put(&self, mut record: BrowserPresence) -> BrowserPresence {
        let mut state = self.inner.lock();
        let revision = state.next_version();
        record.revision = revision;
        state.tombstones.remove(&record.endpoint_id);
        state.active.insert(record.endpoint_id.clone(), record.clone());
        self.counters
            .upserts_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        record
    }

    fn remove(&self, endpoint_id: &str) -> Option<BrowserPresence> {
        let mut state = self.inner.lock();
        let record = state.active.remove(endpoint_id)?;
        let revision = state.next_version();
        state.tombstones.insert(endpoint_id.to_string(), revision);
        state.prune_tombstones(hive_core::now_ms());
        self.counters
            .clears_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(record)
    }

    fn expire(&self, now: u64) -> Vec<BrowserPresence> {
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

    fn snapshot(&self) -> PresenceSnapshot {
        self.inner.lock().clone()
    }

    /// Merge an incoming snapshot into local state **per endpoint id** — never
    /// as a wholesale replacement.
    ///
    /// This used to be `*state = incoming` behind an `incoming.version <=
    /// state.version` gate, and that silently DESTROYED presence records.
    /// Every node's `version` is wall-clock anchored (`next_version()` is
    /// `now_ms()`), so two nodes that each admitted a browser hold snapshots
    /// whose versions differ by milliseconds; whichever replicated with the
    /// higher version overwrote the other node's entire map, and the browser
    /// admitted through the losing node vanished from the constellation with
    /// no error logged anywhere. Presence is a per-endpoint fact owned by
    /// whichever node the browser admitted through — with 14 nodes behind
    /// round-robin DNS that is routinely a different node per browser — so the
    /// join has to be per endpoint id, not per snapshot.
    ///
    /// Rules, applied to the union of both sides: a record wins on the higher
    /// `revision`; a tombstone at or above a record's revision always beats
    /// that record (so revocation/expiry still propagates as authoritative
    /// deletion — the "replicate zero satellites now" property `store_sync`'s
    /// REGISTRY comment depends on); tombstones union by max revision. A
    /// record that is dropped WITHOUT a tombstone (a node that lost state)
    /// can be resurrected by a peer here, which is deliberate and bounded:
    /// `expires_ms` is a 90s TTL and `list` filters on it, so a genuinely
    /// dead record ages out on its own rather than being destroyed early.
    fn adopt(&self, incoming: PresenceSnapshot) -> Option<(PresenceSnapshot, PresenceSnapshot)> {
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
            let PresenceSnapshot {
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

        state.version = state.version.max(incoming.version);
        state.prune_tombstones(hive_core::now_ms());

        // Preserve the caller's no-op signal: store_sync treats `None` as
        // "nothing adopted", and a merge that changed nothing must not be
        // reported as a change.
        if state.active == old.active && state.tombstones == old.tombstones {
            return None;
        }
        let new = state.clone();
        Some((old, new))
    }
}

impl Default for BrowserPresenceStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Server-side coordinate sanitation: bounds, finiteness, and the coarse
/// quantization floor. `None` in, `None` out — a client that declines
/// location (or sends nothing) never gets an accidental (0,0) placement.
fn sanitize_location(lat: Option<f64>, lon: Option<f64>, accuracy_km: Option<f64>) -> Option<(f64, f64, f64)> {
    let (lat, lon) = (lat?, lon?);
    if !lat.is_finite() || !lon.is_finite() || !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let quant = |v: f64| (v / GEO_QUANT_DEGREES).round() * GEO_QUANT_DEGREES;
    let accuracy = accuracy_km
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(MAX_ACCURACY_KM)
        .clamp(MIN_ACCURACY_KM, MAX_ACCURACY_KM);
    Some((quant(lat), quant(lon), accuracy))
}

#[derive(Deserialize)]
struct PresenceRequest {
    endpoint_id: String,
    #[serde(default)]
    lat: Option<f64>,
    #[serde(default)]
    lon: Option<f64>,
    #[serde(default)]
    accuracy_km: Option<f64>,
    #[serde(default)]
    relay_hint: String,
    state: String,
}

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new().route(
        "/v1/browser/presence",
        get(list_presence).post(upsert_presence),
    )
    .route("/v1/browser/presence/:endpoint_id", axum::routing::delete(clear_presence))
}

fn claims_required(claims: Claims) -> Result<crate::auth::Claims, (StatusCode, String)> {
    claims.map(|claims| claims.0).ok_or((
        StatusCode::UNAUTHORIZED,
        "a verified platform session is required".into(),
    ))
}

/// A presence record may only exist alongside a live admission the caller
/// owns for the same endpoint — presence never grants serving on its own,
/// and this ties its lifecycle to admission revocation for free.
fn require_owned_admission(
    cloud: &Arc<CloudState>,
    claims: &crate::auth::Claims,
    endpoint_id: &str,
) -> Result<String, (StatusCode, String)> {
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let admission = crate::browser_admission::local_admission(&cloud, &tenant, endpoint_id, hive_core::now_ms())
        .ok_or((
            StatusCode::NOT_FOUND,
            "no active browser admission for this endpoint".into(),
        ))?;
    if admission.subject != claims.sub && !matches!(claims.role.as_str(), "owner" | "admin") {
        return Err((
            StatusCode::FORBIDDEN,
            "cannot publish presence for another user's browser session".into(),
        ));
    }
    Ok(tenant)
}

async fn upsert_presence(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Json(request): Json<PresenceRequest>,
) -> ApiResult {
    let claims = claims_required(claims)?;
    if !VALID_STATES.contains(&request.state.as_str()) {
        return Err((StatusCode::BAD_REQUEST, "invalid presence state".into()));
    }
    let tenant = require_owned_admission(&cloud, &claims, &request.endpoint_id)?;
    let located = sanitize_location(request.lat, request.lon, request.accuracy_km);
    let now = hive_core::now_ms();
    let display_label = format!(
        "{}-{}",
        tenant,
        request.endpoint_id.get(..8).unwrap_or(&request.endpoint_id)
    );
    // Both identity facts are computed HERE, from the proven endpoint id and
    // the verified session — never from `request`, which carries no field for
    // either. `platform_admin` is independently derived by the token minter
    // against this backend's own `admin_emails` set (see `admin::mint_token`),
    // so it is not a client assertion either.
    let node_name = node_name(&request.endpoint_id);
    let shard_eligible = claims.platform_admin || is_fleet_browser_node(&claims.sub);
    let record = BrowserPresence {
        endpoint_id: request.endpoint_id,
        tenant,
        subject: claims.sub,
        display_label,
        node_name,
        shard_eligible,
        lat: located.map(|(lat, _, _)| lat),
        lon: located.map(|(_, lon, _)| lon),
        accuracy_km: located.map(|(_, _, accuracy)| accuracy),
        located_ms: located.map(|_| now),
        relay_hint: request.relay_hint.chars().take(128).collect(),
        state: request.state,
        issued_ms: now,
        expires_ms: now.saturating_add(PRESENCE_TTL_SECS.saturating_mul(1_000)),
        revision: 0,
    };
    let record = cloud.browser_presence.put(record);
    echo_presence_to_leader(&cloud, &record);
    Ok(Json(json!({ "presence": record })))
}

/// Push a freshly-written presence record straight to the control-plane leader.
///
/// Presence is written on whichever node the browser posted to, and otherwise
/// only reaches the rest of the fleet on the periodic `store_sync` snapshot. That
/// is too slow to be the ONLY path: a record lives 90s, and a browser that
/// republishes every ~60s can have its record repeatedly miss the snapshot window
/// on remote nodes, so an operator watching the constellation sees browser nodes
/// blink in and out — or, for a node whose beat never lands in a snapshot, never
/// appear at all. Witnessed: fc-bangkok publishing presence 10s earlier, live and
/// serving, absent from two consecutive dashboard reads.
///
/// The leader specifically, not every node: `list_presence` has followers merge
/// the leader's view into their own, so making the leader complete makes every
/// reader complete — at ONE rpc per beat instead of one per node per beat.
///
/// Best-effort by construction: it is an accelerator in front of `store_sync`,
/// never a correctness dependency. A failed echo costs latency, not data — the
/// snapshot still carries the record — so this never blocks or fails the write.
fn echo_presence_to_leader(cloud: &Arc<CloudState>, record: &BrowserPresence) {
    if cloud.is_control_plane_leader() {
        return;
    }
    let leader = cloud.control_plane_leader();
    if leader.is_empty() || leader == cloud.node_name {
        return;
    }
    let Some((peer_id, addr)) = cloud
        .registry
        .nodes()
        .iter()
        .find(|n| n.name == leader && n.healthy)
        .and_then(|n| Some((n.peer_id.clone()?, n.iroh_addr.clone()?)))
    else {
        return;
    };
    let Ok(body) = serde_json::to_vec(record) else {
        return;
    };
    let cloud = cloud.clone();
    let endpoint_id = record.endpoint_id.clone();
    tokio::spawn(async move {
        let ok = crate::gossip::request_to(
            &cloud,
            &peer_id,
            &addr,
            hive_p2p::GOSSIP_POST,
            "/v1/browser/presence/mesh-echo",
            &body,
            5,
        )
        .await
        .is_some();
        tracing::debug!(%endpoint_id, ok, "browser presence leader echo");
    });
}

/// Receiving side of [`echo_presence_to_leader`], dispatched from
/// `gossip::dispatch`'s `/v1/browser/presence/mesh-echo` POST arm.
///
/// Applies through the same `put` every local write uses, so the store's own
/// last-writer rule decides: an echo can never overwrite a newer record, and a
/// replayed or out-of-order echo is inert rather than corrupting. The record is
/// taken VERBATIM — every field on it was already derived server-side from a
/// verified session on the originating node (`put_presence` computes
/// `node_name`, `shard_eligible` and `tenant` itself and never reads them from
/// the request), and the mesh transport is peer-authenticated.
pub fn mesh_echo(cloud: &Arc<CloudState>, body: &[u8]) -> Vec<u8> {
    let Ok(record) = serde_json::from_slice::<BrowserPresence>(body) else {
        return Vec::new();
    };
    cloud.browser_presence.put(record);
    Vec::new()
}

async fn list_presence(State(cloud): State<Arc<CloudState>>, claims: Claims) -> ApiResult {
    let claims = claims_required(claims)?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let local = cloud.browser_presence.list(&tenant, hive_core::now_ms());

    // A follower consults the leader whenever it is not the leader — NOT only
    // when its own view is empty.
    //
    // The `local.is_empty()` trigger this replaces is the round-robin-reads bug
    // in its subtlest form: presence is written on whichever node the browser
    // posted to and then replicates on a periodic store_sync snapshot, so a
    // follower routinely holds a PARTIAL set. Returning that partial set is
    // indistinguishable, to the caller, from a complete one — and because the
    // public dashboard host is round-robin DNS, consecutive loads land on
    // different followers and show different subsets of the same fleet.
    // Witnessed: two reads 30s apart returned 4 records then 6, with a live,
    // serving fc-bangkok browser node absent from both, which reads to an
    // operator as "my browser nodes disappeared".
    //
    // Merged, not replaced: a browser posts to its LOCAL node, so this node can
    // hold a record the leader has not received yet, while the leader holds the
    // records this node has not. Union by endpoint id, newest `issued_ms` wins —
    // the same last-writer rule `put` applies, so a merge can never resurrect a
    // record that a newer write superseded.
    if !cloud.is_control_plane_leader() {
        let leader = cloud.control_plane_leader();
        if let Some(value) =
            crate::admin::fetch_from_host(&cloud, &leader, "/v1/browser/presence", &tenant).await
        {
            let remote: Vec<BrowserPresence> = value
                .get("presence")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            if !remote.is_empty() {
                let mut merged: BTreeMap<String, BrowserPresence> = BTreeMap::new();
                for record in local.into_iter().chain(remote) {
                    match merged.get(&record.endpoint_id) {
                        Some(seen) if seen.issued_ms >= record.issued_ms => {}
                        _ => {
                            merged.insert(record.endpoint_id.clone(), record);
                        }
                    }
                }
                let merged: Vec<BrowserPresence> = merged.into_values().collect();
                return Ok(Json(json!({ "presence": merged })));
            }
        }
        // Leader unreachable, or it genuinely knows nothing: the local view is
        // still the honest best answer, never an error.
        return Ok(Json(json!({ "presence": local })));
    }
    Ok(Json(json!({ "presence": local })))
}

async fn clear_presence(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Path(endpoint_id): Path<String>,
) -> ApiResult {
    let claims = claims_required(claims)?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let current = cloud
        .browser_presence
        .get(&tenant, &endpoint_id, hive_core::now_ms())
        .ok_or((StatusCode::NOT_FOUND, "no presence record".into()))?;
    if current.subject != claims.sub && !matches!(claims.role.as_str(), "owner" | "admin") {
        return Err((
            StatusCode::FORBIDDEN,
            "cannot clear another user's presence".into(),
        ));
    }
    cloud.browser_presence.remove(&endpoint_id);
    Ok(Json(json!({ "ok": true, "cleared": endpoint_id })))
}

/// Called from `browser_admission::remove_endpoint` so a revoked/expired
/// admission always takes its presence record down with it — presence must
/// never outlive the admission that authorized it.
pub fn remove_for_endpoint(cloud: &Arc<CloudState>, endpoint_id: &str) {
    cloud.browser_presence.remove(endpoint_id);
}

pub fn snapshot_bytes(cloud: &Arc<CloudState>) -> Vec<u8> {
    if cloud.is_control_plane_leader() {
        cloud.browser_presence.expire(hive_core::now_ms());
    }
    serde_json::to_vec(&cloud.browser_presence.snapshot()).unwrap_or_default()
}

pub fn adopt_snapshot(cloud: &Arc<CloudState>, bytes: &[u8]) -> Option<usize> {
    let incoming: PresenceSnapshot = serde_json::from_slice(bytes).ok()?;
    let (_, new) = cloud.browser_presence.adopt(incoming)?;
    Some(new.active.len())
}

pub fn mesh_list(cloud: &Arc<CloudState>, tenant: &str) -> Vec<u8> {
    let records = cloud
        .browser_presence
        .list(&crate::admin::norm(tenant), hive_core::now_ms());
    serde_json::to_vec(&json!({ "presence": records })).unwrap_or_default()
}
