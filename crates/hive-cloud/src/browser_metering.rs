//! Per-tenant browser relay BYTE metering (bn-impl-relay-byte-metering).
//!
//! Distinct from the per-REQUEST metering in billing.rs
//! (`browser_requests`): this module counts the FRAMED bytes a browser
//! endpoint moves through a fleet node — the input for any future
//! fairness/quota/pricing decision, since a browser serving through the relay
//! costs the fleet roughly 3x the bytes of node-serving (the relay pays both
//! legs). METERING ONLY: these counters feed no rate card and no bill.
//!
//! Byte counting happens inside hive-p2p at the two browser boundaries
//! (inbound `serve_browser_conn`, outbound `BrowserPool::request_op`) and is
//! reported per ENDPOINT ID through `hive_p2p::set_browser_meter`. The
//! endpoint→tenant join happens HERE, at record time, against this node's own
//! admission store (`live_for_endpoint` — the same re-check the CRR exchange
//! uses): an endpoint whose admission expired or was revoked mid-connection
//! lands in the `_unattributed` bucket rather than being silently dropped or
//! misattributed.
//!
//! Counters are NODE-LOCAL (a sidecar under `persist::data_dir()`, the
//! `dns_geo.json` precedent — never `PlatformSnapshot`, never replicated) and
//! the read fans out like `admin::fleet_function_stats`: the operator view
//! merges this node's counters with every healthy peer's
//! `/v1/browser/metering?local=true` over `admin::fetch_from_host`.

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;
type ApiResult = Result<Json<Value>, (StatusCode, String)>;

/// Node-local persistence sidecar — the `dns_geo.json` precedent: derived
/// node-local data must not ride the platform-state write path or replicate.
const METER_FILE: &str = "browser_metering.json";
/// Bumped only if the row shape changes incompatibly; anything else is
/// ignored wholesale rather than half-interpreted.
const FORMAT_VERSION: u32 = 1;
/// Refuse to read a file larger than this — a corrupt sidecar must never
/// become an OOM at boot. Bounded tenants × ~100 bytes is nowhere near this.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Tenant bucket for bytes whose endpoint had no live admission at record
/// time (expired/revoked mid-connection, or a witness harness with no
/// admission store entry). Never silently dropped, never guessed.
const UNATTRIBUTED: &str = "_unattributed";

/// Debounce interval for the background saver. `HIVE_BROWSER_METER_SAVE_MS=0`
/// turns persistence off entirely (no load, no save). Default 10s bounds an
/// unclean-kill loss to a few seconds of counters — metering, not a ledger.
fn save_interval() -> Option<std::time::Duration> {
    let ms = std::env::var("HIVE_BROWSER_METER_SAVE_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(10_000);
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// One tenant's framed byte totals on THIS node. `reports` counts meter
/// callbacks (one per completed read/write stage — a full request+reply
/// exchange reports twice), a sanity signal for exactness witnesses.
#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize)]
struct TenantMeter {
    #[serde(rename = "in")]
    inbound_bytes: u64,
    #[serde(rename = "out")]
    outbound_bytes: u64,
    #[serde(rename = "rep")]
    reports: u64,
    #[serde(rename = "first_ms")]
    first_seen_ms: u64,
    #[serde(rename = "last_ms")]
    last_seen_ms: u64,
}

#[derive(Default)]
struct MeterState {
    tenants: BTreeMap<String, TenantMeter>,
    dirty: bool,
}

/// The process-wide meter. A module-static (never a CloudState field) keeps
/// this feature's whole footprint inside this file; hive-p2p's meter callback
/// is itself process-global, so the pairing is exact.
static METER: std::sync::OnceLock<BrowserMeter> = std::sync::OnceLock::new();

pub struct BrowserMeter {
    inner: parking_lot::Mutex<MeterState>,
}

impl BrowserMeter {
    fn load() -> Self {
        let meter = Self {
            inner: parking_lot::Mutex::new(MeterState::default()),
        };
        for (tenant, row) in load_from_disk() {
            meter.inner.lock().tenants.insert(tenant, row);
        }
        meter
    }

    fn record(&self, tenant: &str, inbound: u64, outbound: u64) {
        let now = hive_core::now_ms();
        let mut state = self.inner.lock();
        let row = state.tenants.entry(tenant.to_string()).or_default();
        row.inbound_bytes = row.inbound_bytes.saturating_add(inbound);
        row.outbound_bytes = row.outbound_bytes.saturating_add(outbound);
        row.reports = row.reports.saturating_add(1);
        if row.first_seen_ms == 0 {
            row.first_seen_ms = now;
        }
        row.last_seen_ms = now;
        state.dirty = true;
    }

    fn snapshot(&self) -> BTreeMap<String, TenantMeter> {
        self.inner.lock().tenants.clone()
    }

    /// Spawn the debounced background saver once per process. Every failure
    /// mode of the write path degrades to a WARN — metering must never take
    /// the node down, and an unsaved tick is folded into the next one.
    fn spawn_saver(&'static self) {
        let Some(interval) = save_interval() else {
            return;
        };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let rows = {
                    let mut state = self.inner.lock();
                    if !state.dirty {
                        continue;
                    }
                    state.dirty = false;
                    state.tenants.clone()
                };
                if let Err(error) = write_rows(rows) {
                    tracing::warn!(error = %error, "browser_metering: save failed (counters keep accruing in memory)");
                }
            }
        });
    }
}

/// Build the hive-p2p meter callback. Registered once at boot next to the
/// admission handler. The endpoint→tenant join runs against this node's own
/// admission store at RECORD time, so a revoked/expired admission can never
/// take its already-moved bytes with it into the void — they land in
/// `_unattributed` instead.
pub fn meter_handler(cloud: &Arc<CloudState>) -> hive_p2p::BrowserMeterHandler {
    let meter: &'static BrowserMeter = METER.get_or_init(BrowserMeter::load);
    meter.spawn_saver();
    let cloud = cloud.clone();
    std::sync::Arc::new(move |endpoint_id: String, inbound: u64, outbound: u64| {
        let tenant = cloud
            .browser_admissions
            .live_for_endpoint(&endpoint_id, hive_core::now_ms())
            .map(|record| record.tenant)
            .unwrap_or_else(|| UNATTRIBUTED.to_string());
        meter.record(&tenant, inbound, outbound);
    })
}

/// This node's own view: the shape every fanout hop returns, and the shape
/// merged into the fleet view's per-node breakdown.
fn local_view(cloud: &Arc<CloudState>) -> Value {
    let tenants = METER
        .get()
        .map(|meter| meter.snapshot())
        .unwrap_or_default();
    json!({
        "node": cloud.node_name,
        "meter": if METER.get().is_some() { "active" } else { "disabled" },
        "tenants": tenants_json(tenants),
    })
}

/// Mesh (gossip::dispatch) read path for the local slice — the iroh-only-peer
/// counterpart of `GET /v1/browser/metering?local=true` with a service
/// delegation. Operator gating already happened on the outer request the
/// caller is fanning out from, exactly like the `/v1/functions` arm.
pub fn mesh_local(cloud: &Arc<CloudState>) -> Vec<u8> {
    serde_json::to_vec(&local_view(cloud)).unwrap_or_default()
}

/// Tenant rows as a stable, bytes-descending JSON array.
fn tenants_json(tenants: BTreeMap<String, TenantMeter>) -> Vec<Value> {
    let mut rows: Vec<Value> = tenants
        .into_iter()
        .map(|(tenant, m)| {
            json!({
                "tenant": tenant,
                "inbound_bytes": m.inbound_bytes,
                "outbound_bytes": m.outbound_bytes,
                "bytes_total": m.inbound_bytes.saturating_add(m.outbound_bytes),
                "reports": m.reports,
                "first_seen_ms": m.first_seen_ms,
                "last_seen_ms": m.last_seen_ms,
            })
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row["bytes_total"].as_u64().unwrap_or_default()));
    rows
}

#[derive(Deserialize)]
struct MeteringQuery {
    /// The internal fanout guard (`/v1/functions` precedent): without it the
    /// fleet view's per-peer fetches would re-fan to every other peer.
    local: Option<bool>,
}

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new().route("/v1/browser/metering", get(browser_metering))
}

async fn browser_metering(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    axum::extract::Query(query): axum::extract::Query<MeteringQuery>,
) -> ApiResult {
    // A verified `service` delegation asking for its own local slice is the
    // internal aggregation path (the `/v1/functions?local=true` precedent) —
    // it answers WITHOUT the operator check because the calling node already
    // enforced operator on the outer request.
    let internal = claims
        .as_ref()
        .map(|extension| extension.0.role == "service")
        .unwrap_or(false);
    if query.local == Some(true) && internal {
        return Ok(Json(local_view(&cloud)));
    }
    crate::admin::require_operator(claims.as_ref().map(|extension| &extension.0))?;
    if query.local == Some(true) {
        let mut view = local_view(&cloud);
        view["scope"] = json!("local");
        return Ok(Json(view));
    }

    // Fleet view: this node's counters merged with every healthy peer's
    // local slice — the `fleet_function_stats` fanout shape. Unreachable or
    // pre-upgrade peers are silently absent (never fail the whole read over
    // one bad node), and are counted honestly in `peers_answered`.
    let mut nodes = vec![local_view(&cloud)];
    let peers: Vec<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|node| node.name != cloud.node_name && node.healthy)
        .map(|node| node.name)
        .collect();
    let polled = peers.len();
    let fetched = futures::future::join_all(peers.iter().map(|name| {
        crate::admin::fetch_from_host(&cloud, name, "/v1/browser/metering?local=true", "")
    }))
    .await;
    let mut answered = 0usize;
    for view in fetched.into_iter().flatten() {
        answered += 1;
        nodes.push(view);
    }

    let mut fleet: BTreeMap<String, TenantMeter> = BTreeMap::new();
    for view in &nodes {
        let Some(rows) = view["tenants"].as_array() else {
            continue;
        };
        for row in rows {
            let Some(tenant) = row["tenant"].as_str() else {
                continue;
            };
            let merged = fleet.entry(tenant.to_string()).or_default();
            merged.inbound_bytes = merged
                .inbound_bytes
                .saturating_add(row["inbound_bytes"].as_u64().unwrap_or_default());
            merged.outbound_bytes = merged
                .outbound_bytes
                .saturating_add(row["outbound_bytes"].as_u64().unwrap_or_default());
            merged.reports = merged
                .reports
                .saturating_add(row["reports"].as_u64().unwrap_or_default());
            let first = row["first_seen_ms"].as_u64().unwrap_or_default();
            if first > 0 && (merged.first_seen_ms == 0 || first < merged.first_seen_ms) {
                merged.first_seen_ms = first;
            }
            merged.last_seen_ms = merged
                .last_seen_ms
                .max(row["last_seen_ms"].as_u64().unwrap_or_default());
        }
    }
    Ok(Json(json!({
        "generated_ms": hive_core::now_ms(),
        "scope": "fleet",
        "billing": "metering only — these counters feed no rate card and no bill",
        "tenants": tenants_json(fleet),
        "nodes": nodes,
        "peers_polled": polled,
        "peers_answered": answered,
    })))
}

// ---------------------------------------------------------------------------
// On-disk form (`$HIVE_DATA/browser_metering.json`). Atomic temp-file +
// fsync + rename, the durability shape every other sidecar uses.
// ---------------------------------------------------------------------------

#[derive(Default, Serialize, Deserialize)]
struct DiskFile {
    #[serde(default)]
    v: u32,
    #[serde(default)]
    saved_ms: u64,
    #[serde(default)]
    tenants: BTreeMap<String, TenantMeter>,
}

fn meter_path() -> std::path::PathBuf {
    crate::persist::data_dir().join(METER_FILE)
}

fn write_rows(tenants: BTreeMap<String, TenantMeter>) -> std::io::Result<()> {
    let dir = crate::persist::data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(METER_FILE);
    let tmp = dir.join(format!("{METER_FILE}.tmp"));
    let file = DiskFile {
        v: FORMAT_VERSION,
        saved_ms: hive_core::now_ms(),
        tenants,
    };
    let json = serde_json::to_string(&file).unwrap_or_else(|_| "{}".into());
    {
        let f = std::fs::File::create(&tmp)?;
        use std::io::Write;
        let mut w = std::io::BufWriter::new(&f);
        w.write_all(json.as_bytes())?;
        w.flush()?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Load persisted counters. EVERY failure mode — missing, unreadable,
/// oversized, malformed, wrong version — degrades to empty counters and at
/// most a WARN: a corrupt scratch file must never become a boot failure.
fn load_from_disk() -> BTreeMap<String, TenantMeter> {
    let empty = BTreeMap::new();
    if save_interval().is_none() {
        return empty;
    }
    let path = meter_path();
    // Absent is the normal first-boot case, not an error worth logging.
    let Ok(meta) = std::fs::metadata(&path) else {
        return empty;
    };
    if meta.len() > MAX_FILE_BYTES {
        tracing::warn!(bytes = meta.len(), path = %path.display(), "browser_metering: file too large; ignoring");
        return empty;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "browser_metering: unreadable; starting empty");
            return empty;
        }
    };
    let file: DiskFile = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "browser_metering: corrupt; starting empty");
            return empty;
        }
    };
    if file.v != FORMAT_VERSION {
        tracing::warn!(
            found = file.v,
            want = FORMAT_VERSION,
            "browser_metering: file version mismatch; starting empty"
        );
        return empty;
    }
    file.tenants
}
