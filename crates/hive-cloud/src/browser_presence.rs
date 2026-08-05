//! Replicated, TTL-bound coarse presence for admitted browser peers.
//!
//! Separate from `browser_admission` (which gates whether a browser endpoint
//! may serve at all) and from `NodeInfo`/the fleet registry — a presence
//! record exists only alongside a live admission for the same endpoint, is
//! never read by placement/scheduling/DNS/health, and carries no exact
//! location: coordinates are quantized server-side regardless of what a
//! client claims, because a client-only quantization step is trivially
//! bypassed by a direct API call.

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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrowserPresence {
    pub endpoint_id: String,
    pub tenant: String,
    pub subject: String,
    /// Tenant-scoped display token — never the raw platform identity.
    pub display_label: String,
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
            .collect()
    }

    fn get(&self, tenant: &str, endpoint_id: &str, now: u64) -> Option<BrowserPresence> {
        self.inner
            .lock()
            .active
            .get(endpoint_id)
            .filter(|record| record.tenant == tenant && record.expires_ms > now)
            .cloned()
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
    let record = BrowserPresence {
        endpoint_id: request.endpoint_id,
        tenant,
        subject: claims.sub,
        display_label,
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
    Ok(Json(json!({ "presence": record })))
}

async fn list_presence(State(cloud): State<Arc<CloudState>>, claims: Claims) -> ApiResult {
    let claims = claims_required(claims)?;
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let local = cloud.browser_presence.list(&tenant, hive_core::now_ms());
    if local.is_empty() && !cloud.is_control_plane_leader() {
        let leader = cloud.control_plane_leader();
        if let Some(value) =
            crate::admin::fetch_from_host(&cloud, &leader, "/v1/browser/presence", &tenant).await
        {
            return Ok(Json(value));
        }
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
