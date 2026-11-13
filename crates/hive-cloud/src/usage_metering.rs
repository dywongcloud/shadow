//! Per-tenant RESOURCE usage metering — a NODE-LOCAL cumulative counter sidecar.
//!
//! This is the `browser_metering.rs` precedent applied to the six billable resource
//! dimensions: `db_row_ops`, `db_storage_byte_ms`, `object_storage_byte_ms`,
//! `ingress_bytes`, `egress_bytes`, `workflow_steps`. It is MEASUREMENT ONLY: it never charges, never writes a
//! ledger, and never runs on a request path. A single background task on the `bulkhead`
//! runtime samples the existing counter/byte boundaries and a debounced saver persists the
//! cumulative rows to `$HIVE_DATA/usage_metering.json`.
//!
//! Node-local by construction. Derived node-local data never rides the platform-state write
//! path and never replicates (the `dns_geo.json` / `browser_metering.json` precedent). The
//! operator fleet view merges this node's rows with each healthy peer's
//! `/v1/usage/metering?local=true` over `admin::fetch_from_host`.
//!
//! Tenant identity is SERVER-DERIVED only: `Database.team` / `ProjectSettings.team` read
//! through `admin::record_tenant`, never `x-hive-team`, never a raw `?team=` query, never a
//! guessed "personal" namespace. Anything unresolved lands in `_unattributed`, which is
//! observable but NEVER billed (the billing fold in `spawn_billing_meter_loop` skips it).
//!
//! ## Dedupe rule: ONE counters source per hop
//!
//! * `db_row_ops`, `ingress_bytes`, `egress_bytes` come from ONE source — the cumulative
//!   `db_metrics::counters_for(db_id)` (`requests`, `bytes_in`, `bytes_out`) — sampled as a
//!   DELTA since the previous tick. These counters are NODE-LOCAL (accumulated on the node
//!   that SERVED the request), so `fleet_totals`' saturating sum over peers is exact: every
//!   node saw a disjoint part of the traffic. We deliberately do NOT add a second byte
//!   counter at the hive-p2p / `browser_metering` relay boundary: those framed relay bytes
//!   are already counted once by `set_browser_meter` for a different (non-billed) purpose,
//!   and counting them here as well would charge the same wire bytes twice.
//! * `db_storage_byte_ms` comes from the physical, node-local managed SQLite files under
//!   `sqlite-dbs/`, attributed by the database record. A file physically exists on one
//!   node, so the fleet sum is exact.
//! * `object_storage_byte_ms` comes from `relational::drive_bytes_used(project)`, a
//!   FLEET-REPLICATED relational mirror that every node reads identically. Sampling it on
//!   every node and summing in `fleet_totals` would multiply-count by the node count, so
//!   exactly ONE hop — the control-plane authority — records it. `fleet_totals` never sums
//!   an already-merged view: it fetches only `?local=true` slices (flat `tenants[]`, no
//!   nested `nodes[]`).
//! * byte·ms is a GAUGE integral: `bytes * elapsed_ms`, added once per sample window.
//!   Cumulative request/byte counters are DELTAS. The two are never mixed.

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;
type ApiResult = Result<Json<Value>, (StatusCode, String)>;

/// Node-local persistence sidecar: derived node-local data must not ride the platform-state
/// write path or replicate.
const METER_FILE: &str = "usage_metering.json";
/// Bumped only on an incompatible row-shape change; anything else is ignored wholesale.
const FORMAT_VERSION: u32 = 1;
/// A corrupt sidecar must never become an OOM at boot.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// UTC-day length in ms for the WORKFLOW step cap window (a stated assumption).
const WORKFLOW_DAY_MS: u64 = 86_400_000;
/// Bytes whose tenant cannot be resolved from a server-derived record: never guessed, never
/// silently dropped, NEVER billed.
pub const UNATTRIBUTED: &str = "_unattributed";

/// Debounce/sample interval. `HIVE_USAGE_METER_SAVE_MS=0` disables persistence AND sampling
/// entirely (no load, no save). Metering is not a ledger: an unclean kill loses at most one
/// window of counters.
fn save_interval() -> Option<std::time::Duration> {
    let ms = std::env::var("HIVE_USAGE_METER_SAVE_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(10_000);
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// One tenant's cumulative resource usage on THIS node. Every field is `#[serde(default)]`:
/// a persisted row from an older shape, or one written mid-upgrade, deserializes to honest
/// zeros rather than failing the whole load.
#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TenantUsage {
    pub db_row_ops: u64,
    pub db_storage_byte_ms: u64,
    pub object_storage_byte_ms: u64,
    pub ingress_bytes: u64,
    pub egress_bytes: u64,
    /// WORKFLOW steps that actually executed, cumulative. serde(default) on the
    /// whole struct keeps pre-workflow persisted rows loading as honest zeros.
    pub workflow_steps: u64,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
}

/// The six measurable dimensions. `record` is the ONLY writer; each kind maps to exactly one
/// field so two kinds can never race onto the same counter.
#[derive(Clone, Copy, Debug)]
pub enum UsageKind {
    DbRowOps,
    DbStorageByteMs,
    ObjectStorageByteMs,
    IngressBytes,
    EgressBytes,
    WorkflowSteps,
}

#[derive(Default)]
struct MeterState {
    tenants: BTreeMap<String, TenantUsage>,
    /// Bounded, node-local per-UTC-day WORKFLOW step window for the plan cap.
    /// Keyed by (tenant, UTC day number); pruned to the current day on each
    /// admission, so a long-lived node holds at most one bucket per tenant that
    /// ran a step today. NOT persisted: a restart re-baselines the window.
    workflow_day: HashMap<(String, u64), u64>,
    dirty: bool,
}

/// The process-wide meter. A module static (never a `CloudState` field) keeps this feature's
/// whole footprint in this file.
static METER: std::sync::OnceLock<UsageMeter> = std::sync::OnceLock::new();

struct UsageMeter {
    inner: parking_lot::Mutex<MeterState>,
}

impl UsageMeter {
    fn load() -> Self {
        let meter = Self {
            inner: parking_lot::Mutex::new(MeterState::default()),
        };
        for (tenant, row) in load_from_disk() {
            meter.inner.lock().tenants.insert(tenant, row);
        }
        meter
    }
}

fn meter() -> &'static UsageMeter {
    METER.get_or_init(UsageMeter::load)
}

/// A tenant string is admissible ONLY when it came from a server-derived record. An empty
/// tag, the fail-closed `__untagged__` sentinel, or the bucket name itself all resolve to
/// `_unattributed` — never to a guessed owner namespace.
fn resolve_tenant(tenant: &str) -> &str {
    let t = tenant.trim();
    if t.is_empty() || t == crate::admin::UNTAGGED_TENANT || t == UNATTRIBUTED {
        UNATTRIBUTED
    } else {
        t
    }
}

/// Adds `delta` to one tenant's cumulative row with saturating arithmetic. `delta` is either
/// a per-window counter DELTA or a byte·ms GAUGE integral (see the module doc); callers must
/// not pass a running total here. Never stalls a request path: one short mutex, no I/O, no
/// await, no panics.
pub fn record(tenant: &str, kind: UsageKind, delta: u64) {
    if delta == 0 {
        return;
    }
    let now = hive_core::now_ms();
    let key = resolve_tenant(tenant).to_string();
    let mut state = meter().inner.lock();
    let row = state.tenants.entry(key).or_default();
    match kind {
        UsageKind::DbRowOps => row.db_row_ops = row.db_row_ops.saturating_add(delta),
        UsageKind::DbStorageByteMs => {
            row.db_storage_byte_ms = row.db_storage_byte_ms.saturating_add(delta)
        }
        UsageKind::ObjectStorageByteMs => {
            row.object_storage_byte_ms = row.object_storage_byte_ms.saturating_add(delta)
        }
        UsageKind::IngressBytes => row.ingress_bytes = row.ingress_bytes.saturating_add(delta),
        UsageKind::EgressBytes => row.egress_bytes = row.egress_bytes.saturating_add(delta),
        UsageKind::WorkflowSteps => row.workflow_steps = row.workflow_steps.saturating_add(delta),
    }
    if row.first_seen_ms == 0 {
        row.first_seen_ms = now;
    }
    row.last_seen_ms = now;
    state.dirty = true;
}

/// Atomically RESERVE one WORKFLOW step against a tenant's per-UTC-day cap.
/// `cap == 0` is unlimited. Returns false (reserving nothing) once the cap is
/// reached, so the caller REFUSES before executing. The window is a stated
/// assumption: node-local, UTC-midnight to UTC-midnight, bounded to today's
/// buckets. This gate never writes a tenant row: measurement of an executed
/// step is a separate `record` call at the ONE step-execution site, so `record`
/// stays the single meter writer.
pub fn workflow_step_admitted(tenant: &str, cap: u64) -> bool {
    let now = hive_core::now_ms();
    let day = now / WORKFLOW_DAY_MS;
    let key = resolve_tenant(tenant).to_string();
    let mut state = meter().inner.lock();
    state.workflow_day.retain(|(_, d), _| *d == day);
    let used = state
        .workflow_day
        .get(&(key.clone(), day))
        .copied()
        .unwrap_or(0);
    if cap > 0 && used >= cap {
        return false;
    }
    let bucket = state.workflow_day.entry((key, day)).or_insert(0);
    *bucket = bucket.saturating_add(1);
    true
}

/// The server-derived tenant for a WORKFLOW step: `resolve_tenant` maps an
/// empty, untagged or unattributed input to UNATTRIBUTED rather than guessing.
pub fn workflow_step_tenant(tenant: &str) -> &str {
    if tenant.trim() == crate::admin::ANON_TENANT {
        return UNATTRIBUTED;
    }
    resolve_tenant(tenant)
}

/// This node's own cumulative rows.
pub fn snapshot() -> BTreeMap<String, TenantUsage> {
    meter().inner.lock().tenants.clone()
}

/// The flat LOCAL slice, shared by `mesh_local()`, the `?local=true` HTTP guard and the
/// per-peer fan-out in `fleet_totals`. LOCAL ROWS ONLY: there is deliberately NO nested
/// `nodes[]` array. A merged view that also carried `nodes[]` would be summed twice by an
/// aggregator that walked both, so the local slice is flat by construction.
fn local_view() -> Value {
    json!({
        "v": FORMAT_VERSION,
        "meter": if METER.get().is_some() { "active" } else { "disabled" },
        "tenants": tenants_json(snapshot()),
    })
}

/// Mesh (`gossip::dispatch`) read path for the local slice: the iroh-only-peer counterpart
/// of `GET /v1/usage/metering?local=true`. Operator gating already happened on the outer
/// request the caller fans out from. Local rows only — never a `nodes[]` breakdown.
pub fn mesh_local() -> Vec<u8> {
    serde_json::to_vec(&local_view()).unwrap_or_default()
}

/// Tenant rows as a stable, bytes-descending JSON array (all six dimensions plus the
/// first/last observation timestamps).
fn tenants_json(tenants: BTreeMap<String, TenantUsage>) -> Vec<Value> {
    let mut rows: Vec<Value> = tenants
        .into_iter()
        .map(|(tenant, u)| {
            let total = u
                .db_row_ops
                .saturating_add(u.db_storage_byte_ms)
                .saturating_add(u.object_storage_byte_ms)
                .saturating_add(u.ingress_bytes)
                .saturating_add(u.egress_bytes)
                .saturating_add(u.workflow_steps);
            json!({
                "tenant": tenant,
                "db_row_ops": u.db_row_ops,
                "db_storage_byte_ms": u.db_storage_byte_ms,
                "object_storage_byte_ms": u.object_storage_byte_ms,
                "ingress_bytes": u.ingress_bytes,
                "egress_bytes": u.egress_bytes,
                "workflow_steps": u.workflow_steps,
                "first_seen_ms": u.first_seen_ms,
                "last_seen_ms": u.last_seen_ms,
                "_total": total,
            })
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row["_total"].as_u64().unwrap_or_default()));
    rows
}

#[derive(Deserialize)]
struct MeteringQuery {
    /// Internal fanout guard: without `?local=true` the fleet view's per-peer fetches would
    /// re-fan to every other peer (the `/v1/functions` precedent).
    local: Option<bool>,
}

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new().route("/v1/usage/metering", get(usage_metering))
}

/// `GET /v1/usage/metering`: operator-only, `?local=true` for one node's slice. A mesh
/// delegation (`auth::is_mesh_internal`: subject AND `service` role, never a tenant key)
/// asking for its own local slice is answered WITHOUT the operator check — the calling node
/// already enforced operator on the outer request.
async fn usage_metering(
    State(cloud): State<Arc<CloudState>>,
    claims: Claims,
    axum::extract::Query(query): axum::extract::Query<MeteringQuery>,
) -> ApiResult {
    let internal = claims
        .as_ref()
        .is_some_and(|extension| crate::auth::is_mesh_internal(&extension.0));
    if query.local == Some(true) && internal {
        return Ok(Json(local_view()));
    }
    crate::admin::require_operator(claims.as_ref().map(|extension| &extension.0))?;
    if query.local == Some(true) {
        let mut view = local_view();
        view["scope"] = json!("local");
        return Ok(Json(view));
    }

    // Fleet view: merge LOCAL slices only. Never sum a view that already merged.
    let peers: Vec<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|node| node.name != cloud.node_name && node.healthy)
        .map(|node| node.name)
        .collect();
    let polled = peers.len();
    let fetched = futures::future::join_all(peers.iter().map(|name| {
        crate::admin::fetch_from_host(&cloud, name, "/v1/usage/metering?local=true", "")
    }))
    .await;
    let mut fleet = snapshot();
    let mut answered = 0usize;
    for view in fetched.into_iter().flatten() {
        answered += 1;
        merge_rows(&mut fleet, &view);
    }
    Ok(Json(json!({
        "generated_ms": hive_core::now_ms(),
        "scope": "fleet",
        "billing": "metering only — folded into billing solely when resource_charging_enabled()",
        "unattributed_never_billed": UNATTRIBUTED,
        "tenants": tenants_json(fleet),
        "peers_polled": polled,
        "peers_answered": answered,
    })))
}

/// Folds ONE peer's flat `tenants[]` slice into the fleet map with saturating_add. Reads the
/// top-level `tenants` array ONLY: a `nodes[]` breakdown is never walked, so a view that
/// already merged can never be double counted. `first_seen_ms` is a min, `last_seen_ms` a
/// max (timestamps do not sum).
fn merge_rows(fleet: &mut BTreeMap<String, TenantUsage>, view: &Value) {
    let Some(rows) = view["tenants"].as_array() else {
        return;
    };
    for row in rows {
        let Some(tenant) = row["tenant"].as_str() else {
            continue;
        };
        let merged = fleet.entry(tenant.to_string()).or_default();
        merged.db_row_ops = merged
            .db_row_ops
            .saturating_add(row["db_row_ops"].as_u64().unwrap_or_default());
        merged.db_storage_byte_ms = merged
            .db_storage_byte_ms
            .saturating_add(row["db_storage_byte_ms"].as_u64().unwrap_or_default());
        merged.object_storage_byte_ms = merged
            .object_storage_byte_ms
            .saturating_add(row["object_storage_byte_ms"].as_u64().unwrap_or_default());
        merged.ingress_bytes = merged
            .ingress_bytes
            .saturating_add(row["ingress_bytes"].as_u64().unwrap_or_default());
        merged.egress_bytes = merged
            .egress_bytes
            .saturating_add(row["egress_bytes"].as_u64().unwrap_or_default());
        merged.workflow_steps = merged
            .workflow_steps
            .saturating_add(row["workflow_steps"].as_u64().unwrap_or_default());
        let first = row["first_seen_ms"].as_u64().unwrap_or_default();
        if first > 0 && (merged.first_seen_ms == 0 || first < merged.first_seen_ms) {
            merged.first_seen_ms = first;
        }
        merged.last_seen_ms = merged
            .last_seen_ms
            .max(row["last_seen_ms"].as_u64().unwrap_or_default());
    }
}

/// Fleet-wide cumulative rows: this node's own `snapshot()` plus every healthy peer's
/// `?local=true` slice, merged with saturating_add. Only flat local slices are ever summed.
/// `_unattributed` stays in the result (observable) but the billing fold skips it.
pub async fn fleet_totals(cloud: &Arc<CloudState>) -> BTreeMap<String, TenantUsage> {
    let mut fleet = snapshot();
    let peers: Vec<String> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|node| node.name != cloud.node_name && node.healthy)
        .map(|node| node.name)
        .collect();
    let fetched = futures::future::join_all(peers.iter().map(|name| {
        crate::admin::fetch_from_host(cloud, name, "/v1/usage/metering?local=true", "")
    }))
    .await;
    for view in fetched.into_iter().flatten() {
        merge_rows(&mut fleet, &view);
    }
    fleet
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct DiskFile {
    v: u32,
    saved_ms: u64,
    tenants: BTreeMap<String, TenantUsage>,
}

fn meter_path() -> std::path::PathBuf {
    crate::persist::data_dir().join(METER_FILE)
}

/// Writes the rows atomically (temp file, fsync, rename), the durability shape every
/// node-local sidecar uses.
fn write_rows(tenants: BTreeMap<String, TenantUsage>) -> std::io::Result<()> {
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

/// Loads persisted rows. EVERY failure mode (missing, unreadable, oversized, malformed,
/// wrong version) degrades to empty rows and at most a WARN: a corrupt scratch file must
/// never become a boot failure. A missing file is the normal first-boot case.
fn load_from_disk() -> BTreeMap<String, TenantUsage> {
    let empty = BTreeMap::new();
    if save_interval().is_none() {
        return empty;
    }
    let path = meter_path();
    let Ok(meta) = std::fs::metadata(&path) else {
        return empty;
    };
    if meta.len() > MAX_FILE_BYTES {
        tracing::warn!(bytes = meta.len(), path = %path.display(), "usage_metering: file too large; ignoring");
        return empty;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "usage_metering: unreadable; starting empty");
            return empty;
        }
    };
    let file: DiskFile = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "usage_metering: corrupt; starting empty");
            return empty;
        }
    };
    if file.v != FORMAT_VERSION {
        tracing::warn!(found = file.v, want = FORMAT_VERSION, "usage_metering: file version mismatch; starting empty");
        return empty;
    }
    file.tenants
}

/// The state the sampler keeps BETWEEN ticks (never persisted: a restarted node re-baselines
/// its cumulative counter deltas honestly from zero).
struct SampleState {
    last_sample_ms: u64,
    last_gauge_ms: u64,
    tick: u64,
    prev_counters: HashMap<String, (u64, u64, u64)>,
}

impl Default for SampleState {
    fn default() -> Self {
        let now = hive_core::now_ms();
        Self {
            last_sample_ms: now,
            last_gauge_ms: now,
            tick: 0,
            prev_counters: HashMap::new(),
        }
    }
}

/// Bounds the elapsed window so a long pause (VM suspend, bulkhead starvation) cannot turn
/// one gauge reading into a month of byte·ms.
fn bounded_dt_ms(from_ms: u64, now_ms: u64) -> u64 {
    now_ms
        .saturating_sub(from_ms)
        .clamp(1, 60_000)
}

/// Samples the EXISTING boundaries. Measurement only: no charge, no ledger, no mutation of
/// platform state, and it runs on the bulkhead runtime so a slow SQL gauge or a directory
/// walk can never stall a serving tokio worker. WORKFLOW steps are NOT sampled here:
/// they are discrete events recorded at the ONE step-execution site (main.rs::wf_invoker)
/// through this same `record` writer, so this sampler invents no second writer for them.
async fn sample_once(cloud: &Arc<CloudState>, state: &mut SampleState) {
    let now = hive_core::now_ms();
    let dt = bounded_dt_ms(state.last_sample_ms, now);
    state.last_sample_ms = now;
    state.tick = state.tick.wrapping_add(1);

    // Hop 1: node-local cumulative DB gateway counters. ONE source for row ops + ingress +
    // egress; deltas only. The node that served the request is the only node that counted it,
    // so the fleet sum is exact.
    let databases = cloud.databases.list(None);
    for db in &databases {
        let tenant = crate::admin::record_tenant(&db.team);
        let c = crate::db_metrics::counters_for(&db.id);
        let entry = state.prev_counters.entry(db.id.clone()).or_insert((0, 0, 0));
        let (p_req, p_in, p_out) = *entry;
        record(tenant, UsageKind::DbRowOps, c.requests.saturating_sub(p_req));
        record(
            tenant,
            UsageKind::IngressBytes,
            c.bytes_in.saturating_sub(p_in),
        );
        record(
            tenant,
            UsageKind::EgressBytes,
            c.bytes_out.saturating_sub(p_out),
        );
        *entry = (c.requests, c.bytes_in, c.bytes_out);
    }

    // Gauges run on a longer cadence: the drive quota is a relational SQL read per project,
    // and byte·ms is an integral over the WHOLE elapsed gauge window.
    let gauge_due = state.tick % 6 == 0;
    if !gauge_due {
        return;
    }
    let gauge_dt = bounded_dt_ms(state.last_gauge_ms, now);
    state.last_gauge_ms = now;

    // Hop 2: managed DB file bytes, physically node-local under `sqlite-dbs/`, attributed
    // by the database RECORD (never a path-derived or header-derived tenant).
    let sqlite_dir = crate::databases::sqlite_store_dir();
    for db in &databases {
        let file = sqlite_dir.join(crate::databases::sqlite_file_name(&db.id));
        if let Ok(meta) = std::fs::metadata(&file) {
            let byte_ms = meta.len().saturating_mul(gauge_dt);
            record(
                crate::admin::record_tenant(&db.team),
                UsageKind::DbStorageByteMs,
                byte_ms,
            );
        }
    }

    // Hop 3: drive/object storage bytes. `relational::drive_bytes_used` reads a
    // FLEET-REPLICATED mirror every node sees identically, so ONLY the single
    // control-plane authority records it; `fleet_totals` then sums at most one non-zero
    // slice and never multiply-counts by the node count. This is a MEASUREMENT gate, not a
    // second charging loop: `may_act(Job::BillingMeter)` remains the ONE charging gate.
    if cloud.is_control_plane_leader() {
        for (project, settings) in cloud.projects.snapshot() {
            let bytes = crate::relational::drive_bytes_used(&project).await;
            if bytes == 0 {
                continue;
            }
            record(
                crate::admin::record_tenant(&settings.team),
                UsageKind::ObjectStorageByteMs,
                bytes.saturating_mul(gauge_dt),
            );
        }
    }
}

/// Spawns ONE debounced background task on the BULKHEAD runtime: it samples the existing
/// boundaries and persists dirty rows each interval. Every failure degrades to a WARN —
/// metering must never take the node down, and an unsaved tick is folded into the next one.
/// Called once at boot from `main.rs`.
pub fn spawn_saver(cloud: Arc<CloudState>) {
    meter(); // load persisted rows eagerly, before the first sample can overwrite them
    // The contract mirror is drained here, NOT in sample_once: this is the one bulkhead loop that
    // runs with CloudState in hand on EVERY node. Billing enqueues events synchronously (from
    // ledger_push / meter_usage) and never awaits an engine fsync on a request path.
    let Some(interval) = save_interval() else {
        // Sidecar persistence disabled is not a reason to stop attesting billing: metering and
        // ledger events are still enqueued by billing.rs, so keep a drain-only loop alive.
        crate::bulkhead::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(10_000)).await;
                let r = crate::db_contract::drain_metering(&cloud).await;
                if r.dropped > 0 || r.persist_errors > 0 {
                    tracing::warn!(?r, "usage_metering: contract mirror drain degraded");
                }
                // Hourly, gated inside: immutable reconciliation record + retention event, both
                // published as signed journal ops. Runs on the bulkhead, never a serving worker.
                crate::db_contract::run_journal_audit(&cloud).await;
            }
        });
        return;
    };
    crate::bulkhead::spawn(async move {
        let mut state = SampleState::default();
        loop {
            tokio::time::sleep(interval).await;
            sample_once(&cloud, &mut state).await;
            // BEFORE the dirty check: a tick with no meter delta can still hold charges, credits
            // and finalized checkpoints that must be attested.
            let mirror = crate::db_contract::drain_metering(&cloud).await;
            if mirror.dropped > 0 || mirror.persist_errors > 0 || mirror.refused > 0 {
                tracing::warn!(?mirror, "usage_metering: contract mirror drain degraded");
            } else if mirror.submitted > 0 {
                tracing::info!(submitted = mirror.submitted, "contract: metering/billing events attested");
            }
            // BEFORE the dirty `continue`: an idle meter still needs its journal audit records.
            crate::db_contract::run_journal_audit(&cloud).await;
            let rows = {
                let mut guard = meter().inner.lock();
                if !guard.dirty {
                    continue;
                }
                guard.dirty = false;
                guard.tenants.clone()
            };
            if let Err(error) = write_rows(rows) {
                tracing::warn!(error = %error, "usage_metering: save failed (counters keep accruing in memory)");
            }
        }
    });
}
