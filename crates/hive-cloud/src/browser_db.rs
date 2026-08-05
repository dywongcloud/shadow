//! Browser-replicated databases: the fleet half of the browser↔fleet live
//! CRR exchange (bn-browser-fleet-crr-exchange, contract:
//! `docs/browser-db-contract.md`; CRR semantics are `hive-crsql`'s contract,
//! named here, never redefined).
//!
//! Per project with a `browser_db` opt-in this module:
//!
//! * owns the fleet replica file
//!   (`$HIVE_DATA/browser-dbs/hive-browserdb-{sanitize_tag(project)}.db` —
//!   platform-templated name, a tenant string never becomes a path
//!   component, the `hive-vol-{project}` discipline) and keeps its schema
//!   applied from the deployment's spec (cr-sqlite v0.17 does not replicate
//!   schema, so both halves derive it from the SAME replicated spec);
//! * serves `Op::CrrSync` rounds to granted browser peers over the
//!   `hive/browser/0` ALPN (installed as hive-p2p's [`BrowserCrrHandler`]):
//!   every request is re-checked against THIS node's own replicated
//!   admission view (live, unexpired, carrying a `db` grant for exactly this
//!   tenant+deployment) AND against the live deployment descriptor still
//!   carrying the block — the `proxy_to_owner` re-check precedent; a foreign
//!   tenant and an unknown project are the identical refusal, no existence
//!   leak;
//! * enforces the caps with typed refusal + whole-batch rollback
//!   (`max_value_bytes` per change, `max_bytes` per replica, both from the
//!   RESOLVED spec — never truncated, which in an LWW store is silent
//!   permanent divergence);
//! * runs the replica GC with the `browser_artifacts::gc` /
//!   `gc_rootfs_images` blast-radius guards verbatim: an empty keep-set
//!   refuses the pass, a reap set over `HIVE_BROWSER_DB_GC_MAX_REAP_FRACTION`
//!   refuses, and only files older than the inert grace window
//!   (`HIVE_BROWSER_DB_INERT_GRACE_SECS`, default 30 days — one bad deploy
//!   removing the block must not nuke production data) AND
//!   `HIVE_BROWSER_DB_GC_GRACE_SECS` reap.
//!
//! Bytes, site ids and watermarks replicate ONLY through the CRR protocol —
//! never `PlatformSnapshot`, never `store_sync`, never a gossip snapshot arm
//! (the `dns_geo.json` precedent), and there is deliberately NO owner-proxy
//! HTTP arm: a file copy is not a merge. The replica file is opened per sync
//! round inside `spawn_blocking` (rusqlite connections are !Send and the
//! work is bounded local disk IO), so this module holds no live connection
//! state between rounds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use fluid_core::{BrowserDbPolicy, ResolvedBrowserDbPolicy};
use fluid_gateway::BrowserScope;
use hive_browser_proto::{
    reset as browser_reset, CrrStatus, CrrSyncReply, BROWSER_MAX_CRR_FRAME,
};
use serde_json::{json, Value};

use crate::state::CloudState;

/// Store layout: `persist::data_dir()/browser-dbs/` — one replica file per
/// opted-in project, named by [`replica_file_name`].
const STORE_DIR_NAME: &str = "browser-dbs";

/// A block-removed project's replica files stay INERT this long before the
/// GC may reap them (contract §5: one bad deploy must not nuke production
/// data). Project deletion rides the same clock — the file's mtime freezes
/// at its last sync either way.
const DEFAULT_INERT_GRACE_SECS: u64 = 30 * 24 * 60 * 60;
const DEFAULT_GC_GRACE_SECS: u64 = 600;
const DEFAULT_GC_MAX_REAP_FRACTION: f64 = 0.5;
const DEFAULT_RECONCILE_SECS: u64 = 30;

/// Soft budget for the export half of one reply: keep the reply comfortably
/// under the wire frame cap after the fixed envelope and the (bounded)
/// watermark advertisement. A batch that cannot fit ANY frame is named in
/// the reply message and stays in the replica (loud, never truncated).
const EXPORT_BUDGET_HEADROOM: usize = 64 * 1024;

pub fn store_dir() -> PathBuf {
    crate::persist::data_dir().join(STORE_DIR_NAME)
}

/// The platform-templated replica file name for a project — the ONLY name
/// the exchange ever opens. `sanitize_tag` maps every character outside
/// `[a-z0-9._-]` to `-` and trims separators, so even a project called `/`
/// or `../../etc` yields a plain file name; the wire carries no file field
/// at all (the name is derived from the server-resolved grant, contract §6).
pub fn replica_file_name(project: &str) -> String {
    format!("hive-browserdb-{}.db", crate::git::sanitize_tag(project))
}

/// The server-derived `db` capability a granted browser reconciles from
/// (admit/renewal response) — see `browser_admission::capability_json`. The
/// browser learns its OPFS file name, its caps, its schema and its dialable
/// fleet peers here and nowhere else; nothing in it is client input.
pub fn capability_db_json(
    cloud: &Arc<CloudState>,
    tenant: &str,
    grant: &crate::browser_admission::BrowserDbGrant,
    resolved: &ResolvedBrowserDbPolicy,
    expires_ms: u64,
) -> Value {
    json!({
        "tenant": tenant,
        "project": grant.project,
        "access": grant.access,
        "max_bytes": resolved.max_bytes,
        "max_value_bytes": resolved.max_value_bytes,
        "db_file": grant.db_file,
        "schema": resolved.schema,
        "sync_peers": sync_peers(cloud),
        "expires_ms": expires_ms,
    })
}

/// The fleet peers a browser may dial for sync rounds: every HEALTHY node's
/// proven iroh identity + dialable address, from the live registry — the
/// exact `trusted_caller_ids` discipline (health-filtered, never node names,
/// never browser ids, never client input), plus the `addr_json` the
/// browser's wasm needs to dial. Bounded: a browser needs a handful of
/// peers, not the whole roster.
fn sync_peers(cloud: &Arc<CloudState>) -> Vec<Value> {
    let mut peers = Vec::new();
    for n in cloud.registry.nodes() {
        if !n.healthy {
            continue;
        }
        let Some(addr_json) = n.iroh_addr.clone() else {
            continue;
        };
        let Some(endpoint_id) = hive_p2p::endpoint_id_from_addr_json(&addr_json) else {
            continue;
        };
        peers.push(json!({ "endpoint_id": endpoint_id, "addr_json": addr_json }));
        if peers.len() >= 8 {
            break;
        }
    }
    peers
}

/// Resolve the deployment descriptor's `browser_db` spec under `tenant`'s
/// ownership — `browser_artifacts::descriptor_for`'s exact two-source shape
/// (local gw records first, then the gossiped `peer_deployments` view).
/// `None` covers BOTH "no such deployment under this tenant" and "not
/// opted-in": callers must never distinguish them to a peer (no existence
/// leak across tenant boundaries).
pub fn db_descriptor_for(
    cloud: &Arc<CloudState>,
    tenant: &str,
    deployment: &str,
) -> Option<(String, BrowserDbPolicy)> {
    for record in cloud.gw.deployment_records() {
        if record.id == deployment
            && record.state == fluid_core::DeployState::Ready
            && crate::admin::record_tenant(&record.tenant) == tenant
        {
            return record
                .manifest
                .browser_db
                .clone()
                .map(|spec| (record.project.clone(), spec));
        }
    }
    for deployments in cloud.peer_deployments.read().values() {
        for info in deployments {
            if info.id.as_str() == deployment
                && info.state == fluid_core::DeployState::Ready
                && crate::admin::record_tenant(&info.tenant) == tenant
            {
                return info
                    .browser_db
                    .clone()
                    .map(|spec| (info.project.clone(), spec));
            }
        }
    }
    None
}

/// What one sync round is allowed to do, resolved fresh per request from the
/// live admission + the live descriptor (never from wire input).
struct RoundGrant {
    project: String,
    resolved: ResolvedBrowserDbPolicy,
    read_only: bool,
}

/// The exchange-side grant re-check (contract §3): the LIVE admission for
/// this endpoint (this node's own replicated store view — unexpired), a `db`
/// grant riding it, and the deployment descriptor STILL carrying the block.
/// Team scope syncs read+write; Public scope syncs read-only and only while
/// the LIVE spec still says `public_read` — a redeploy flipping it off cuts
/// public grants on the next request, not the next renewal.
fn resolve_round_grant(
    cloud: &Arc<CloudState>,
    endpoint_id: &str,
) -> Option<RoundGrant> {
    let admission =
        cloud
            .browser_admissions
            .live_for_endpoint(endpoint_id, hive_core::now_ms())?;
    admission.db.as_ref()?;
    let (project, spec) = db_descriptor_for(cloud, &admission.tenant, &admission.deployment)?;
    let resolved = spec.resolve();
    let read_only = match admission.scope {
        BrowserScope::Team => false,
        BrowserScope::Public => {
            if !resolved.public_read {
                return None;
            }
            true
        }
    };
    Some(RoundGrant {
        project,
        resolved,
        read_only,
    })
}

/// Installable `Op::CrrSync` handler for `hive_p2p::serve_tunnels_full`.
pub fn crr_sync_handler(cloud: &Arc<CloudState>) -> hive_p2p::BrowserCrrHandler {
    let cloud = cloud.clone();
    Arc::new(move |remote_id: String, payload: Vec<u8>| {
        let cloud = cloud.clone();
        Box::pin(async move { handle_crr_sync(&cloud, &remote_id, payload).await })
    })
}

async fn handle_crr_sync(
    cloud: &Arc<CloudState>,
    remote_id: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>, u32> {
    // Grant re-check against THIS node's own replicated admission view +
    // live descriptor. No-admission, no-grant, foreign-tenant and
    // block-removed all collapse to the identical refusal — a browser peer
    // learns nothing about what exists (contract §3).
    let Some(grant) = resolve_round_grant(cloud, remote_id) else {
        tracing::warn!(remote = %remote_id, "browser db sync refused: no live db grant");
        return Err(browser_reset::FORBIDDEN);
    };
    let request =
        hive_browser_proto::split_crr_sync_request(&payload).map_err(|e| e.reset_code())?;
    let path = store_dir().join(replica_file_name(&grant.project));
    // The wire `db_file` is a grant IDENTIFIER, never a path: it must equal
    // the name this node derived from its own server-resolved grant, or a
    // stale capability (the endpoint was re-admitted to a different
    // deployment since) would contaminate another project's replica. The
    // refusal is the identical FORBIDDEN — no existence leak either way.
    if request.db_file != replica_file_name(&grant.project) {
        tracing::warn!(
            remote = %remote_id,
            "browser db sync refused: db_file does not match the live grant"
        );
        return Err(browser_reset::FORBIDDEN);
    }
    let project = grant.project.clone();
    let reply = tokio::task::spawn_blocking(move || sync_round(&path, &grant, &request))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, project = %project, "browser db sync worker join failed");
            browser_reset::HANDLER_FAILED
        })??;
    Ok(hive_browser_proto::encode_crr_sync_reply(&reply))
}

/// One full anti-entropy round against the replica file: apply the
/// requester's push batches (typed refusals, whole-batch rollback), then
/// export everything the requester advertised it is missing, bounded to the
/// reply frame. Runs synchronously on a blocking thread — rusqlite
/// connections are !Send and this is bounded local disk IO.
fn sync_round(
    path: &std::path::Path,
    grant: &RoundGrant,
    request: &hive_browser_proto::CrrSyncRequest,
) -> Result<CrrSyncReply, u32> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            tracing::warn!(error = %e, dir = %parent.display(), "browser db: create store dir failed");
            browser_reset::HANDLER_FAILED
        })?;
    }
    let conn = hive_crsql::open(path).map_err(|e| {
        // The loadable extension missing on this host is an OPERATOR story
        // (fleet packaging, HIVE_CRSQL_EXTENSION_PATH) — name it loudly.
        tracing::warn!(error = %e, file = %path.display(), "browser db: replica open failed (cr-sqlite extension present?)");
        browser_reset::HANDLER_FAILED
    })?;
    ensure_schema(&conn, &grant.resolved).map_err(|e| {
        tracing::warn!(error = %e, file = %path.display(), "browser db: schema apply failed");
        browser_reset::HANDLER_FAILED
    })?;

    let mut status = CrrStatus::Ok;
    let mut message = String::new();

    // ---- apply half (write direction) ----
    if !request.batches.is_empty() {
        if grant.read_only {
            // Contract §3: read-only means the fleet NEVER applies a change
            // originating from this grant. Typed refusal; the export half
            // below is still served.
            status = CrrStatus::ReadOnly;
            message = "grant is read-only; push batches refused".to_string();
        } else {
            let (apply_status, apply_message) =
                apply_push_batches(&conn, path, &grant.resolved, &request.batches);
            status = apply_status;
            message = apply_message;
        }
    }

    // ---- watermark acknowledgement ----
    let watermarks = hive_crsql::known_sites(&conn).map_err(|e| {
        tracing::warn!(error = %e, file = %path.display(), "browser db: watermark read failed");
        browser_reset::HANDLER_FAILED
    })?;

    // ---- export half (read direction) ----
    let advertised: BTreeMap<&[u8], i64> = request
        .watermarks
        .iter()
        .map(|(site, version)| (site.as_slice(), *version))
        .collect();
    let mut budget = BROWSER_MAX_CRR_FRAME.saturating_sub(EXPORT_BUDGET_HEADROOM);
    let mut batches: Vec<Vec<u8>> = Vec::new();
    let mut more = false;
    let mut skipped_oversized = 0usize;
    let mut first_oversized: Option<(String, String)> = None;
    'sites: for (site, version) in &watermarks {
        let since = advertised.get(site.as_slice()).copied().unwrap_or(0);
        if *version <= since {
            continue;
        }
        let site_batches = hive_crsql::changes_since_site(
            &conn,
            site,
            since,
            hive_crsql::DEFAULT_MAX_BATCH_CHANGES,
        )
        .map_err(|e| {
            tracing::warn!(error = %e, file = %path.display(), "browser db: export failed");
            browser_reset::HANDLER_FAILED
        })?;
        for mut batch in site_batches {
            // Value cap at the export boundary too (contract §4, both
            // directions): an oversized value stays in this replica and is
            // named loudly — never truncated, never wedging the rest of the
            // batch. Anchors (since/max) are unchanged, so the receiver's
            // watermark still advances past the skipped value.
            let before = batch.changes.len();
            batch.changes.retain(|c| {
                let over = hive_crsql::val_payload_bytes(&c.val) > grant.resolved.max_value_bytes;
                if over && first_oversized.is_none() {
                    first_oversized = Some((c.table.clone(), hive_crsql::hex(&c.pk)));
                }
                !over
            });
            skipped_oversized += before - batch.changes.len();
            let encoded = batch.encode();
            if encoded.len() > budget {
                // Frame-full: the requester re-requests and its freshly
                // applied watermarks are the resume cursor. A batch that
                // cannot fit ANY frame (one giant commit) stays local,
                // named — it can never travel this wire.
                more = true;
                if batches.is_empty() {
                    message = format!(
                        "{}batch of site {} ({} bytes) exceeds the wire frame; it stays local until split at the origin",
                        if message.is_empty() { "" } else { "; " },
                        hive_crsql::hex(&batch.site_id),
                        encoded.len()
                    );
                }
                break 'sites;
            }
            budget -= encoded.len();
            batches.push(encoded);
        }
    }
    if skipped_oversized > 0 {
        let detail = match &first_oversized {
            Some((table, pk)) => format!("first: table {table} pk {pk}"),
            None => String::new(),
        };
        if !message.is_empty() {
            message.push_str("; ");
        }
        message.push_str(&format!(
            "skipped {skipped_oversized} change(s) over max_value_bytes {} ({detail})",
            grant.resolved.max_value_bytes
        ));
    }

    Ok(CrrSyncReply {
        status,
        more,
        message,
        watermarks,
        batches,
    })
}

/// The apply half shared by the browser-dialed round ([`sync_round`]) and
/// the fleet-dialed operator pull ([`operator_sync_round`]): decode each
/// batch (an undecodable frame is a typed `BatchRefused`, not a crash),
/// enforce the value cap and the replica quota BEFORE each apply (typed
/// refusal, whole-batch rollback — never truncation), apply per batch
/// through `hive_crsql::apply_batch` (gap/replay semantics intact), stop at
/// the first typed refusal. `Ok` means every batch applied or replayed.
fn apply_push_batches(
    conn: &hive_crsql::rusqlite::Connection,
    path: &std::path::Path,
    resolved: &ResolvedBrowserDbPolicy,
    batches: &[Vec<u8>],
) -> (CrrStatus, String) {
    for raw in batches {
        let batch = match hive_crsql::ChangeBatch::decode(raw) {
            Ok(batch) => batch,
            Err(_) => return (CrrStatus::BatchRefused, "undecodable HCB1 batch".to_string()),
        };
        if let Some(c) = batch
            .changes
            .iter()
            .find(|c| hive_crsql::val_payload_bytes(&c.val) > resolved.max_value_bytes)
        {
            return (
                CrrStatus::ValueTooLarge,
                format!(
                    "change value of {} bytes exceeds max_value_bytes {} (table {} pk {})",
                    hive_crsql::val_payload_bytes(&c.val),
                    resolved.max_value_bytes,
                    c.table,
                    hive_crsql::hex(&c.pk)
                ),
            );
        }
        let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let estimate = raw.len() as u64;
        if file_bytes >= resolved.max_bytes
            || file_bytes.saturating_add(estimate) > resolved.max_bytes
        {
            return (
                CrrStatus::QuotaExceeded,
                format!(
                    "replica is {} bytes; applying a {}-byte batch from site {} would exceed max_bytes {}",
                    file_bytes,
                    estimate,
                    hive_crsql::hex(&batch.site_id),
                    resolved.max_bytes
                ),
            );
        }
        match hive_crsql::apply_batch(conn, &batch) {
            Ok(_outcome) => {}
            Err(error) => {
                if let Some(gap) = error.downcast_ref::<hive_crsql::SyncGap>() {
                    return (
                        CrrStatus::SyncGap,
                        format!(
                            "sync gap for site {}: batch continues from {} but durable watermark is {}",
                            hive_crsql::hex(&gap.site_id),
                            gap.batch_since,
                            gap.watermark
                        ),
                    );
                }
                tracing::warn!(error = %error, file = %path.display(), "browser db: apply failed");
                return (CrrStatus::BatchRefused, format!("apply failed: {error}"));
            }
        }
    }
    (CrrStatus::Ok, String::new())
}

/// The fleet-DIALED direction of the exchange: one bounded pull round
/// against an admitted browser, driven by an operator. The fleet sends its
/// watermark advertisement with an EMPTY push (a pure pull: the browser's
/// responder exports everything the fleet is missing, bounded to one
/// frame), then applies the reply's batches to the replica with the same
/// caps and grant checks as the browser-dialed path. v1 sync cadence is
/// browser-driven; this exists so the fleet-initiated arm
/// (`hive_p2p::BrowserPool::crr_sync`) has a real, witnessed caller — and
/// as the operator's on-demand anti-entropy poke. The HTTP response carries
/// METADATA only (counts, watermarks, status) — DB bytes never leave the
/// CRR protocol (contract §2: no HTTP arm serves DB bytes).
async fn operator_sync_round(
    cloud: &Arc<CloudState>,
    endpoint_id: &str,
) -> Result<Value, (axum::http::StatusCode, String)> {
    let grant = resolve_round_grant(cloud, endpoint_id).ok_or((
        axum::http::StatusCode::NOT_FOUND,
        "no live browser db grant for this endpoint".to_string(),
    ))?;
    let admission = cloud
        .browser_admissions
        .live_for_endpoint(endpoint_id, hive_core::now_ms())
        .expect("resolve_round_grant just saw this admission");
    let pool = cloud.browser_mesh.read().clone().ok_or((
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "browser mesh is not up on this node".to_string(),
    ))?;
    let path = store_dir().join(replica_file_name(&grant.project));
    let resolved = grant.resolved.clone();
    // This node's watermarks, read off its own replica — the pull selector
    // the browser's responder exports against.
    let watermarks = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<(Vec<u8>, i64)>, (axum::http::StatusCode, String)> {
            let conn = hive_crsql::open(&path).map_err(|e| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("replica open failed: {e}"),
                )
            })?;
            hive_crsql::known_sites(&conn).map_err(|e| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("watermark read failed: {e}"),
                )
            })
        })
        .await
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("worker join failed: {e}"),
            )
        })??
    };
    let request = hive_browser_proto::encode_crr_sync_request(&hive_browser_proto::CrrSyncRequest {
        db_file: replica_file_name(&grant.project),
        push_more: false,
        watermarks,
        batches: Vec::new(),
    });
    let reply_bytes = pool
        .crr_sync(endpoint_id, &admission.addr_json, &request)
        .await
        .map_err(|e| {
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!("browser sync round failed: {}", e.message),
            )
        })?;
    let reply = hive_browser_proto::split_crr_sync_reply(&reply_bytes).map_err(|e| {
        (
            axum::http::StatusCode::BAD_GATEWAY,
            format!("browser sync reply malformed: {e}"),
        )
    })?;
    let reply_batch_count = reply.batches.len();
    let reply_more = reply.more;
    let applied = tokio::task::spawn_blocking(move || -> Result<(CrrStatus, String, usize), (axum::http::StatusCode, String)> {
        let conn = hive_crsql::open(&path).map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("replica open failed: {e}"),
            )
        })?;
        ensure_schema(&conn, &resolved).map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("schema apply failed: {e}"),
            )
        })?;
        let (status, message) = apply_push_batches(&conn, &path, &resolved, &reply.batches);
        let watermarks = hive_crsql::known_sites(&conn).map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("watermark read failed: {e}"),
            )
        })?;
        Ok((status, message, watermarks.len()))
    })
    .await
    .map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("worker join failed: {e}"),
        )
    })??;
    let status_name = match applied.0 {
        CrrStatus::Ok => "ok",
        CrrStatus::SyncGap => "sync_gap",
        CrrStatus::QuotaExceeded => "quota_exceeded",
        CrrStatus::ValueTooLarge => "value_too_large",
        CrrStatus::ReadOnly => "read_only",
        CrrStatus::BatchRefused => "batch_refused",
    };
    Ok(json!({
        "endpoint_id": endpoint_id,
        "status": status_name,
        "message": applied.1,
        "reply_batches": reply_batch_count,
        "reply_more": reply_more,
        "replica_sites": applied.2,
    }))
}

/// `POST /v1/browser/dbs/sync/:endpoint_id` (operator-only fleet-initiated
/// pull round, see [`operator_sync_round`]) plus the Storages page's
/// tenant-scoped live-status read (see [`project_db_status_http`]).
pub fn routes() -> axum::Router<Arc<CloudState>> {
    axum::Router::new()
        .route(
            "/v1/browser/dbs/sync/:endpoint_id",
            axum::routing::post(operator_sync_http),
        )
        .route(
            "/v1/projects/:project/browser-db/status",
            axum::routing::get(project_db_status_http),
        )
}

/// `GET /v1/projects/:project/browser-db/status` — the Storages page's live
/// status panel: resolved caps plus THIS node's own local replica figures
/// (byte usage, distinct site count, file mtime as a last-write proxy).
///
/// Declaring which side of the round-robin-reads-vs-leader-writes split this
/// is on (AGENTS.md): the replica FILE is node-local storage, which is
/// usually exactly the footgun that rule warns about (write lands on one
/// node, GET served by whichever node round-robin DNS/admin_ingress's
/// local-GET rule picks). It does not apply here — `spawn_reconcile` runs on
/// EVERY node and the descriptor it reconciles from
/// (`DeploymentInfo::browser_db`) is gossiped fleet-wide, so every node that
/// has ever heard of this project's opt-in already maintains its OWN replica
/// of it (see this file's module docs). Whichever node answers this request
/// has real, local data — never a stale zero. Figures can disagree slightly
/// node-to-node until the next anti-entropy round; that is the CRR model
/// converging, not a bug (the same per-observer caveat as health verdicts).
async fn project_db_status_http(
    axum::extract::State(cloud): axum::extract::State<Arc<CloudState>>,
    headers: axum::http::HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::extract::Path(project): axum::extract::Path<String>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, String)> {
    crate::admin::require_project(&cloud, &headers, claims.as_ref().map(|e| &e.0), &project)?;
    let Some(policy) = cloud.projects.get(&project).browser_db else {
        return Ok(axum::Json(json!({ "opted_in": false })));
    };
    let resolved = policy.resolve();
    let path = store_dir().join(replica_file_name(&project));
    let (exists, bytes, sites, last_modified_ms) = match std::fs::metadata(&path) {
        Ok(meta) => {
            let sites = {
                let path = path.clone();
                tokio::task::spawn_blocking(move || {
                    let conn = hive_crsql::open(&path).ok()?;
                    hive_crsql::known_sites(&conn).ok()
                })
                .await
                .ok()
                .flatten()
                .map(|v| v.len())
                .unwrap_or(0)
            };
            let last_modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64);
            (true, meta.len(), sites, last_modified_ms)
        }
        // Not-yet-reconciled (first `HIVE_BROWSER_DB_RECONCILE_SECS` tick
        // hasn't run on this node yet) or genuinely never opened — either
        // way, an honest "no replica here yet", never a fabricated zero-cap.
        Err(_) => (false, 0u64, 0usize, None),
    };
    Ok(axum::Json(json!({
        "opted_in": true,
        "max_bytes": resolved.max_bytes,
        "max_value_bytes": resolved.max_value_bytes,
        "public_read": resolved.public_read,
        "tables": resolved.schema.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
        "notes": resolved.notes,
        "replica": {
            "exists": exists,
            "bytes": bytes,
            "sites": sites,
            "last_modified_ms": last_modified_ms,
        },
    })))
}

async fn operator_sync_http(
    axum::extract::State(cloud): axum::extract::State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::extract::Path(endpoint_id): axum::extract::Path<String>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, String)> {
    crate::admin::require_operator(claims.map(|c| c.0).as_ref())?;
    let value = operator_sync_round(&cloud, &endpoint_id).await?;
    Ok(axum::Json(value))
}
/// Idempotent schema bring-up for a replica: every spec table's DDL, then
/// `crsql_as_crr`. cr-sqlite v0.17 does not replicate schema inside
/// `crsql_changes`, so both replica halves derive it from the SAME
/// replicated spec — this is the fleet half (the browser half gets it
/// verbatim in the admission's `db` capability block).
fn ensure_schema(
    conn: &hive_crsql::rusqlite::Connection,
    resolved: &ResolvedBrowserDbPolicy,
) -> anyhow::Result<()> {
    for table in &resolved.schema {
        conn.execute_batch(&table.ddl)?;
        hive_crsql::as_crr(conn, &table.name)?;
    }
    Ok(())
}

/// Every opted-in project visible to this node: local Ready records whose
/// manifest carries the block, plus the gossiped peer view — the same two
/// sources [`db_descriptor_for`] consults. Keyed by project (database
/// identity IS the project, contract §1); the spec from the local record
/// wins on a tie (fresher than the peer view).
fn opted_in_projects(cloud: &Arc<CloudState>) -> BTreeMap<String, BrowserDbPolicy> {
    let mut out = BTreeMap::new();
    for deployments in cloud.peer_deployments.read().values() {
        for info in deployments {
            if info.state == fluid_core::DeployState::Ready {
                if let Some(spec) = &info.browser_db {
                    out.entry(info.project.clone()).or_insert_with(|| spec.clone());
                }
            }
        }
    }
    for record in cloud.gw.deployment_records() {
        if record.state == fluid_core::DeployState::Ready {
            if let Some(spec) = &record.manifest.browser_db {
                out.insert(record.project.clone(), spec.clone());
            }
        }
    }
    out
}

/// Periodic reconcile: for every opted-in project, ensure the replica file
/// exists with its schema applied (creating an empty replica is also how a
/// replacement node starts its backfill — browsers then carry it up to
/// watermark via the normal protocol). Every node runs this (the replica is
/// per-host, like the podman lock pool / browser-artifact GC precedent).
fn reconcile_replicas(cloud: &Arc<CloudState>) {
    for (project, spec) in opted_in_projects(cloud) {
        let path = store_dir().join(replica_file_name(&project));
        if path.exists() {
            continue;
        }
        let resolved = spec.resolve();
        let result = (|| -> anyhow::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let conn = hive_crsql::open(&path)?;
            ensure_schema(&conn, &resolved)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                tracing::info!(project = %project, file = %path.display(), "browser db: replica created")
            }
            Err(error) => tracing::warn!(
                project = %project,
                %error,
                "browser db: replica create/schema failed (cr-sqlite extension present?)"
            ),
        }
    }
}

/// Replica GC, contract §5 with the `browser_artifacts::gc` /
/// `gc_rootfs_images` blast-radius discipline verbatim:
///
/// * keep-set = every live opted-in project's replica file name. An EMPTY
///   keep-set refuses the pass outright — a state-vs-caller-bug ambiguity no
///   GC may resolve by deleting (drain the store dir by hand if a full,
///   deliberate teardown is ever wanted).
/// * a candidate reaps only when it is NOT in the keep-set AND its mtime is
///   older than BOTH the inert grace window (block-removed/project-deleted
///   retention) and `HIVE_BROWSER_DB_GC_GRACE_SECS`.
/// * a reap set over `HIVE_BROWSER_DB_GC_MAX_REAP_FRACTION` of the store
///   refuses the whole pass — a bug and an unrecoverable one differ by
///   exactly this check.
fn gc_replicas(cloud: &Arc<CloudState>) {
    let keep: BTreeSet<String> = opted_in_projects(cloud)
        .keys()
        .map(|project| replica_file_name(project))
        .collect();
    if keep.is_empty() {
        return;
    }
    let inert_grace = env_u64("HIVE_BROWSER_DB_INERT_GRACE_SECS", DEFAULT_INERT_GRACE_SECS);
    let grace_secs = env_u64("HIVE_BROWSER_DB_GC_GRACE_SECS", DEFAULT_GC_GRACE_SECS);
    let max_fraction = std::env::var("HIVE_BROWSER_DB_GC_MAX_REAP_FRACTION")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0 && *v <= 1.0)
        .unwrap_or(DEFAULT_GC_MAX_REAP_FRACTION);
    let min_age = inert_grace.max(grace_secs);
    let now = std::time::SystemTime::now();
    let dir = store_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut total = 0usize;
    let mut candidates: Vec<(PathBuf, u64)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("hive-browserdb-") || !name.ends_with(".db") {
            continue;
        }
        total += 1;
        if keep.contains(name) {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if age >= min_age {
            candidates.push((path, age));
        }
    }
    if candidates.is_empty() {
        return;
    }
    if candidates.len() as f64 > max_fraction * total as f64 {
        tracing::warn!(
            candidates = candidates.len(),
            total,
            max_fraction,
            "browser db gc: reap fraction exceeded — refusing the whole pass"
        );
        return;
    }
    for (path, age) in &candidates {
        match std::fs::remove_file(path) {
            Ok(()) => tracing::info!(
                file = %path.display(),
                age_days = age / 86_400,
                "browser db gc: reaped inert replica past its grace window"
            ),
            Err(error) => {
                tracing::warn!(file = %path.display(), %error, "browser db gc: reap failed")
            }
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// Registered in main.rs next to the other reconcile loops. The first real
/// pass runs a full interval after boot (persisted deployments are long
/// restored by then — the same double-skip discipline as
/// `browser_artifacts::spawn_gc_loop`).
pub fn spawn_reconcile(cloud: Arc<CloudState>) {
    let interval_secs = env_u64("HIVE_BROWSER_DB_RECONCILE_SECS", DEFAULT_RECONCILE_SECS);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        tick.tick().await;
        tick.tick().await;
        loop {
            reconcile_replicas(&cloud);
            gc_replicas(&cloud);
            tick.tick().await;
        }
    });
}
