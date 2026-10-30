//! Replicated, TTL-bound coarse presence for admitted browser peers. Separate from
//! `browser_admission` (which gates whether a browser endpoint may serve at all) and from
//! `NodeInfo`/the fleet registry: a record exists only alongside a live admission for the same
//! endpoint, is never read by placement/scheduling/DNS/health, and carries no exact location
//! (coordinates are quantized server-side whatever a client claims, since a client-only step is
//! bypassed by a direct API call).
//!
//! Two identity facts on every record are derived SERVER-SIDE, never client input: [`node_name`],
//! the peer's map/legend identity, a pure function of its PROVEN endpoint id (the QUIC public key
//! the admission was minted against), stable across reconnects and prefixed `bn-` so it is never
//! mistaken for a fleet node; and `shard_eligible`, whether this peer may hold fragments of GLOBAL
//! platform state (`browser_db`'s shard planner), stamped from the authenticated caller's
//! `platform_admin` claim at upsert.

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

/// Coarse grid step in degrees (~55 km at the equator, tighter near the poles): enough to place a
/// satellite dot on a world map without revealing a home address. Applied server-side
/// unconditionally; a client's own precision claim is never trusted past this floor.
const GEO_QUANT_DEGREES: f64 = 0.5;
const MIN_ACCURACY_KM: f64 = 25.0;
const MAX_ACCURACY_KM: f64 = 20_000.0;
const PRESENCE_TTL_SECS: u64 = 90;
const TOMBSTONE_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;
const VALID_STATES: [&str; 4] = ["starting", "online", "degraded", "suspended"];

/// Domain separator for [`node_name`]'s digest, so the naming hash never collides with any other
/// digest the platform computes over the same id.
const NODE_NAME_DOMAIN: &str = "hive-browser-node-name-v1\u{0}";

/// The `bn-` prefix is load-bearing: fleet node names are operator-chosen (`fc-virginia`, `sj2`,
/// `node-b`), and a browser peer's name must be unmistakable on the same diagram, legend and log
/// line. Nothing else in the platform mints a name in this shape.
const NODE_NAME_PREFIX: &str = "bn-";

/// 32 x 32 = 1024 mnemonic pairs, x 65536 tags = ~67M display names, kept short (7 chars or fewer
/// each) so `bn-<adj>-<noun>-<tag>` fits under a cube on the constellation. Both lists are FIXED:
/// changing an entry renames every browser that hashed to it, breaking the "stable across
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

/// Subject prefix the fleet-hosted headless browser nodes mint under
/// (`scripts/hive-browser-node-broker.mjs`: `fleet-browser-node:<node>`).
const FLEET_BROWSER_NODE_SUBJECT_PREFIX: &str = "fleet-browser-node:";

/// Whether this session belongs to one of the FLEET'S OWN headless browser nodes, which are
/// shard-eligible without being `platform_admin` (the broker deliberately mints least-privilege,
/// role=member and platform_admin=false, so `claims.platform_admin` alone left the operator-run
/// browser nodes permanently out of `shard_members` and membership was whatever human admin tabs
/// were open). Not a privilege hole: the subject is not client-assertable. It is minted only
/// through `admin::mint_token`, which requires `x-hive-internal == HIVE_INTERNAL_TOKEN`
/// (constant-time, FAILS CLOSED when unconfigured); the broker reads that secret from a root-only
/// file and the dashboard proxy strips a client-supplied `x-hive-internal`, so only a process
/// already running as root on a fleet host can produce it, and anyone able to forge it already
/// holds the fleet's internal credential.
fn is_fleet_browser_node(subject: &str) -> bool {
    subject
        .strip_prefix(FLEET_BROWSER_NODE_SUBJECT_PREFIX)
        .is_some_and(|node| !node.is_empty())
}

/// The stable, human-readable map identity of a browser peer, derived only from its PROVEN endpoint
/// id (`bn-teal-otter-9c1f`). **Server-derived**: the endpoint id is the QUIC public key the
/// admission was minted against, so a browser cannot pick, spoof or squat a name (no field on the
/// wire for it). **Stable across reconnects**: same identity in, same name out, with no clock,
/// counter or membership input. **Distinguishable from a fleet node** by [`NODE_NAME_PREFIX`] and
/// the fixed three-part shape, and short (22 characters or fewer, renderable under a 9px cube).
///
/// It is a DISPLAY name, never an identity key: two endpoint ids can land on the same mnemonic
/// (birthday-bounded at ~67M), so every record keeps `endpoint_id` and every lookup keys on it. The
/// tag is the endpoint id's own leading hex where possible (an operator can eyeball-match
/// `bn-teal-otter-9c1f` against `9c1f…` in the admissions table), padded from the digest only for
/// an id too short or too exotic to supply four.
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

/// Repairs a record on the way OUT of the store: fills `node_name` when empty (a record written by
/// a pre-`node_name` leader, or adopted from such a peer). Applied on every read path (HTTP list,
/// single get, mesh list, shard membership), so no consumer special-cases an anonymous browser
/// peer. It is a recomputation, not a fallback: the name is a pure function of `endpoint_id`, so
/// the value is exactly what the issuing node would have written. `shard_eligible` is deliberately
/// NOT repaired: it encodes an authorization decision made against a live session, and nothing on
/// the record can re-derive it.
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
    /// The peer's own node identity on the constellation ([`node_name`]). `serde(default)` for the
    /// rollout, and EMPTY is repaired rather than tolerated: [`hydrate`] recomputes it on every
    /// read, so a record replicated by a pre-upgrade leader still renders a name (the repaired
    /// value is bit-identical to what the issuing node would have written).
    #[serde(default)]
    pub node_name: String,
    /// May this peer hold fragments of GLOBAL platform state (`browser_db::shard_plan`)? Stamped
    /// from the authenticated caller's `platform_admin` claim at upsert, never from the request
    /// body. `false` is the safe default in every direction: a pre-upgrade record carries no field
    /// and is ineligible (absent capability, never wrong capability), and a non-admin tenant's
    /// browser is ineligible by construction. See `browser_db`'s shard-trust block for why the bar
    /// is platform-admin today and what would have to exist to lower it.
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
    /// Live gauge (not cumulative): currently-active, non-expired records grouped by `state`
    /// ("starting"/"online"/"degraded"/"suspended"), a fleet-wide aggregate only, never per-tenant,
    /// answering "how many browser peers are online right now" without naming any.
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

    /// Every live record eligible to hold GLOBAL state fragments, ACROSS tenants: the shard
    /// planner's membership set (`browser_db::shard_members`). Deliberately not tenant-filtered
    /// (platform state is not a tenant's data); what keeps that sound is the `shard_eligible` gate,
    /// which only a `platform_admin` session can set, plus the caller's own live-admission
    /// re-check. `starting`, `degraded` and `suspended` are excluded: a peer not serving right now
    /// must not be planned against, or every transient blip reshuffles the map. Sorted by endpoint
    /// id so every node derives the identical membership from the identical replicated state.
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
        state
            .active
            .insert(record.endpoint_id.clone(), record.clone());
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

    /// Merges an incoming snapshot into local state **per endpoint id**, never as a wholesale
    /// replacement. A `*state = incoming` behind a version gate silently DESTROYED records:
    /// versions are wall-clock anchored (`next_version()` is `now_ms()`), so two nodes that each
    /// admitted a browser hold snapshots whose versions differ by milliseconds and the higher one
    /// overwrote the other's whole map, making the browser admitted through the losing node vanish
    /// from the constellation with no error. Presence is a per-endpoint fact owned by whichever
    /// node the browser admitted through (routinely a different one per browser behind round-robin
    /// DNS).
    ///
    /// Rules over the union: a record wins on the higher `revision`; a tombstone at or above a
    /// record's revision always beats it (revocation/expiry still propagates as deletion, the
    /// "replicate zero satellites now" property `store_sync`'s REGISTRY depends on); tombstones
    /// union by max revision. A record dropped WITHOUT a tombstone (a node that lost state) can be
    /// resurrected by a peer, deliberately and boundedly: `expires_ms` is a 90 s TTL and `list`
    /// filters on it. A merge that changed nothing returns `None`, the no-op signal `store_sync`
    /// reads as "nothing adopted".
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
fn sanitize_location(
    lat: Option<f64>,
    lon: Option<f64>,
    accuracy_km: Option<f64>,
) -> Option<(f64, f64, f64)> {
    let (lat, lon) = (lat?, lon?);
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
    {
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
    Router::new()
        .route(
            "/v1/browser/presence",
            get(list_presence).post(upsert_presence),
        )
        .route(
            "/v1/browser/presence/:endpoint_id",
            axum::routing::delete(clear_presence),
        )
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
    let admission = crate::browser_admission::local_admission(
        &cloud,
        &tenant,
        endpoint_id,
        hive_core::now_ms(),
    )
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

/// Publishes a presence record for an admitted browser endpoint. Both identity facts are computed
/// HERE, from the proven endpoint id and the verified session, never from `request` (which carries
/// no field for either): `node_name` is a pure function of the endpoint id, and `shard_eligible`
/// follows the session's `platform_admin` claim (independently derived by the token minter against
/// this backend's own `admin_emails`, see `admin::mint_token`) or a fleet browser node's mint
/// subject. The record is echoed to every healthy peer ([`echo_presence_to_peers`]).
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
    echo_presence_to_peers(&cloud, &record);
    Ok(Json(json!({ "presence": record })))
}

/// Pushes a freshly written presence record to EVERY healthy peer, not just the leader. Presence is
/// written on whichever node the browser posted to and otherwise reaches the fleet only on the
/// periodic `store_sync` snapshot, too slow to be the ONLY path (a record lives 90 s and a browser
/// republishes every ~60 s, so beats repeatedly miss the snapshot window and operators see browser
/// nodes blink in and out). Echoing to the leader alone holds only while the leader does not
/// change, and it changes on every fleet restart, exactly when browser nodes re-admit under fresh
/// identities: each node then holds what it saw directly plus whatever the leader-of-the-moment
/// collected, and `adopt` merges per key, so the fragments never reconcile and which node
/// round-robin picks decides what the operator sees. Fanning out makes every node's view complete
/// (a handful of small rpcs per minute per node, spawned and never awaited). Best-effort: an
/// accelerator in front of `store_sync`, never a correctness dependency (a failed echo costs
/// latency, not data).
fn echo_presence_to_peers(cloud: &Arc<CloudState>, record: &BrowserPresence) {
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
    let Ok(body) = serde_json::to_vec(record) else {
        return;
    };
    let cloud = cloud.clone();
    let endpoint_id = record.endpoint_id.clone();
    tokio::spawn(async move {
        let mut ok = 0usize;
        let total = targets.len();
        for (peer_id, addr) in targets {
            if crate::gossip::request_to(
                &cloud,
                &peer_id,
                &addr,
                hive_p2p::GOSSIP_POST,
                "/v1/browser/presence/mesh-echo",
                &body,
                5,
            )
            .await
            .is_some()
            {
                ok += 1;
            }
        }
        tracing::debug!(%endpoint_id, ok, total, "browser presence peer echo");
    });
}

/// Receiving side of [`echo_presence_to_peers`], dispatched from `gossip::dispatch`'s
/// `/v1/browser/presence/mesh-echo` POST arm. Applies through the same `put` every local write
/// uses, so the store's last-writer rule decides: an echo can never overwrite a newer record, and a
/// replayed or out-of-order echo is inert. The record is taken VERBATIM: every field was already
/// derived server-side from a verified session on the originating node (`upsert_presence` computes
/// `node_name`, `shard_eligible` and `tenant` itself and never reads them from the request), and
/// the mesh transport is peer-authenticated.
pub fn mesh_echo(cloud: &Arc<CloudState>, body: &[u8]) -> Vec<u8> {
    let Ok(record) = serde_json::from_slice::<BrowserPresence>(body) else {
        return Vec::new();
    };
    cloud.browser_presence.put(record);
    Vec::new()
}

/// Lists presence for the caller's tenant. A follower consults the leader whenever it is not the
/// leader, NOT only when its own view is empty: presence replicates on a periodic store_sync
/// snapshot, so a follower routinely holds a PARTIAL set that looks complete, and round-robin
/// dashboard DNS showed different subsets of one fleet on consecutive loads. The result is MERGED,
/// not replaced (a browser posts to its LOCAL node, so this node can hold records the leader lacks
/// and vice versa): union by endpoint id, newest `issued_ms` wins, the last-writer rule `put`
/// applies, so a merge never resurrects a record a newer write superseded. An unreachable or empty
/// leader leaves the local view, the honest best answer, never an error.
async fn list_presence(State(cloud): State<Arc<CloudState>>, claims: Claims) -> ApiResult {
    let claims = claims_required(claims)?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let local = cloud.browser_presence.list(&tenant, hive_core::now_ms());

    if let Some(leader) = cloud.leader_forward_target() {
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
    if crate::leadership::may_act(cloud, crate::leadership::Job::BrowserExpiry) {
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
