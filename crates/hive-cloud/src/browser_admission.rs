//! Fresh-session admission for low-trust browser serving peers.
//!
//! Browser identities never enter the fleet registry or trusted peer set. The
//! control-plane leader owns this short-lived store; followers adopt versioned
//! snapshots and only use the records to program Gateway's browser target layer.

use axum::{
    extract::{Path, State},
    http::StatusCode,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserAdmission {
    pub endpoint_id: String,
    pub addr_json: String,
    pub deployment: String,
    pub function: String,
    pub digest: String,
    pub tenant: String,
    pub subject: String,
    pub issued_ms: u64,
    pub expires_ms: u64,
    pub revision: u64,
    #[serde(default)]
    pub scope: BrowserScope,
    pub protocol_version: u16,
}

impl BrowserAdmission {
    fn target(&self) -> BrowserTarget {
        BrowserTarget {
            tenant: self.tenant.clone(),
            deployment: self.deployment.clone(),
            function: self.function.clone(),
            endpoint_id: self.endpoint_id.clone(),
            addr_json: self.addr_json.clone(),
            digest: self.digest.clone(),
            expires_ms: self.expires_ms,
            scope: self.scope,
        }
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
    /// as if IT made the authoritative decision could race ahead of a leader
    /// whose clock lags by even a few ms, causing a later genuinely-
    /// authoritative leader snapshot to be rejected by `adopt()`'s version
    /// check. The narrower operation (denylist only) is safe regardless.
    fn mark_denied(&self, endpoint_id: &str, now: u64) {
        let mut state = self.inner.lock();
        state.denylist.insert(endpoint_id.to_string(), now);
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

    fn adopt(
        &self,
        mut incoming: BrowserAdmissionSnapshot,
    ) -> Option<(BrowserAdmissionSnapshot, BrowserAdmissionSnapshot)> {
        for (id, revision) in &incoming.tombstones {
            if incoming
                .active
                .get(id)
                .is_some_and(|record| record.revision <= *revision)
            {
                incoming.active.remove(id);
            }
        }
        let mut state = self.inner.lock();
        let local_empty = state.active.is_empty() && state.tombstones.is_empty();
        if incoming.version < state.version && !local_empty {
            return None;
        }
        if incoming.version == state.version && !local_empty {
            return None;
        }
        let old = state.clone();
        *state = incoming.clone();
        Some((old, incoming))
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
    deployment: String,
    function: String,
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

fn fresh_user_claims(claims: Claims) -> Result<crate::auth::Claims, (StatusCode, String)> {
    let claims = claims_required(claims)?;
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
        return Err((
            StatusCode::FORBIDDEN,
            "browser admission requires a fresh interactive user session".into(),
        ));
    }
    let iat = claims.iat as u64;
    let exp = claims.exp as u64;
    if exp <= now || iat > now.saturating_add(skew) || now.saturating_sub(iat) > max_age {
        return Err((
            StatusCode::UNAUTHORIZED,
            "browser admission session is expired or not fresh".into(),
        ));
    }
    Ok(claims)
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
) -> Result<(), (StatusCode, String)> {
    let window_ms = std::env::var("HIVE_BROWSER_POP_WINDOW_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(30_000);
    let now = hive_core::now_ms();
    let age = now.abs_diff(challenge_ms);
    if age > window_ms {
        return Err((
            StatusCode::UNAUTHORIZED,
            "browser admission proof-of-possession challenge is stale".into(),
        ));
    }
    let public_key: iroh::PublicKey = endpoint_id.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "browser endpoint id is not a valid ed25519 public key".into(),
        )
    })?;
    let mut sig_bytes = [0u8; 64];
    hex::decode_to_slice(signature_hex, &mut sig_bytes).map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            "browser admission proof-of-possession signature is not valid hex".into(),
        )
    })?;
    let signature = iroh::Signature::from_bytes(&sig_bytes);
    let message = format!("{endpoint_id}:{challenge_ms}");
    public_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                "browser admission proof-of-possession signature does not verify — caller does not control this endpoint's key".into(),
            )
        })
}

fn validate_request(
    cloud: &Arc<CloudState>,
    claims: &crate::auth::Claims,
    request: &AdmissionRequest,
) -> Result<(String, u64), (StatusCode, String)> {
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
            return Err((
                StatusCode::UPGRADE_REQUIRED,
                "protocol_too_old: this browser bundle is outdated; reload to update".into(),
            ));
        }
        hive_browser_proto::ProtocolFit::TooNew => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "protocol_too_new: this node hasn't rolled forward to your protocol version yet; retrying will reach an upgraded node".into(),
            ));
        }
        hive_browser_proto::ProtocolFit::Supported => {}
    }
    if request.addr_json.len() > MAX_ADDR_JSON_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "browser endpoint address is too large".into(),
        ));
    }
    let endpoint_id = hive_p2p::endpoint_id_from_addr_json(&request.addr_json).ok_or((
        StatusCode::BAD_REQUEST,
        "browser endpoint address is malformed".into(),
    ))?;
    if endpoint_id != request.endpoint_id {
        return Err((
            StatusCode::BAD_REQUEST,
            "browser endpoint id does not match its signed address".into(),
        ));
    }
    verify_proof_of_possession(&endpoint_id, request.challenge_ms, &request.signature)?;
    if !hive_browser_proto::valid_function_digest(&request.digest)
        || request.deployment.is_empty()
        || request.deployment.len() > 256
        || request.function.is_empty()
        || request.function.len() > 256
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid browser function target".into(),
        ));
    }
    if request.scope == BrowserScope::Public && !matches!(claims.role.as_str(), "owner" | "admin") {
        return Err((
            StatusCode::FORBIDDEN,
            "public browser serving requires a team owner or admin".into(),
        ));
    }
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    if !deployment_serves(cloud, &tenant, &request.deployment, &request.function) {
        return Err((
            StatusCode::NOT_FOUND,
            "no ready deployment function exists in this tenant".into(),
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
        return Err((
            StatusCode::UNAUTHORIZED,
            "platform session expires before the minimum browser lease".into(),
        ));
    }
    Ok((tenant, expires_ms))
}

fn deployment_serves(
    cloud: &Arc<CloudState>,
    tenant: &str,
    deployment: &str,
    function: &str,
) -> bool {
    let local = cloud.gw.deployment_records().into_iter().any(|record| {
        record.id == deployment
            && record.state == fluid_core::DeployState::Ready
            && crate::admin::record_tenant(&record.tenant) == tenant
            && record.manifest.functions.iter().any(|f| f.name == function)
    });
    local
        || cloud.peer_deployments.read().values().any(|deployments| {
            deployments.iter().any(|record| {
                record.id.as_str() == deployment
                    && record.state == fluid_core::DeployState::Ready
                    && crate::admin::record_tenant(&record.tenant) == tenant
                    && record.functions.iter().any(|name| name == function)
            })
        })
}

async fn admit(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    Json(request): Json<AdmissionRequest>,
) -> ApiResult {
    let claims = fresh_user_claims(claims)?;
    let (tenant, expires_ms) = validate_request(&cloud, &claims, &request)?;
    let issued_ms = hive_core::now_ms();
    let mut record = BrowserAdmission {
        endpoint_id: request.endpoint_id,
        addr_json: request.addr_json,
        deployment: request.deployment,
        function: request.function,
        digest: request.digest,
        tenant,
        subject: claims.sub,
        issued_ms,
        expires_ms,
        revision: 0,
        scope: request.scope,
        protocol_version: request.protocol_version,
    };
    let old = cloud
        .browser_admissions
        .put(record.clone())
        .map_err(|message| (StatusCode::CONFLICT, message.into()))?;
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
    if let Err(error) = cloud.gw.upsert_browser_target(record.target()) {
        cloud
            .browser_admissions
            .revoke(&record.tenant, &record.endpoint_id);
        return Err((StatusCode::INTERNAL_SERVER_ERROR, error.into()));
    }
    Ok(Json(json!({ "admission": record })))
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

fn routing_identity_changed(old: &BrowserAdmission, new: &BrowserAdmission) -> bool {
    old.addr_json != new.addr_json
        || old.deployment != new.deployment
        || old.function != new.function
        || old.digest != new.digest
        || old.tenant != new.tenant
        || old.scope != new.scope
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
                if let Err(error) = cloud.gw.upsert_browser_target(record.target()) {
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
