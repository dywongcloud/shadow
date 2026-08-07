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
use serde::{Deserialize, Serialize};
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

/// The deployment whose `browser_db` block a node with NO explicitly chosen
/// target should replicate (browser-auto-serve-eligible-set) — `None` unless
/// the tenant's answer is UNAMBIGUOUS.
///
/// A browser holds exactly ONE replica (one OPFS file, one lane), so "serve
/// everything automatically" has an honest automatic answer for databases only
/// when the tenant has exactly one project carrying a block. With two or more
/// the platform would be choosing, on recency, WHICH of a tenant's databases
/// gets copied into a donor's browser — and for a Public-scope donor, which
/// one strangers get a read-only replica of. That is a decision the picker
/// must keep making explicitly, so this returns `None` and the node runs with
/// no database grant rather than an arbitrary one.
///
/// Within the single opted-in project the newest Ready deployment wins (its
/// spec is the current one). Same two replicated sources and the same tenant
/// gate as [`db_descriptor_for`].
pub fn auto_db_deployment_for_tenant(
    cloud: &Arc<CloudState>,
    tenant: &str,
    endpoint_id: &str,
) -> Option<String> {
    // project -> (created_at_ms, deployment id) of the newest Ready deployment
    // carrying a block.
    let mut by_project: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut note = |project: String, created: u64, id: String| {
        let entry = by_project.entry(project).or_insert((created, id.clone()));
        if created > entry.0 {
            *entry = (created, id);
        }
    };
    for record in cloud.gw.deployment_records() {
        if record.state != fluid_core::DeployState::Ready
            || crate::admin::record_tenant(&record.tenant) != tenant
            || record.manifest.browser_db.is_none()
        {
            continue;
        }
        note(
            record.project.clone(),
            record.created_at_ms,
            record.id.clone(),
        );
    }
    for deployments in cloud.peer_deployments.read().values() {
        for info in deployments {
            if info.state != fluid_core::DeployState::Ready
                || crate::admin::record_tenant(&info.tenant) != tenant
                || info.browser_db.is_none()
            {
                continue;
            }
            note(info.project.clone(), info.created_at_ms, info.id.0.clone());
        }
    }
    // Nothing to replicate is the only honest "no grant" — the tenant has no
    // project carrying a browser_db block.
    if by_project.is_empty() {
        return None;
    }
    // With MORE than one candidate this used to refuse outright (`len() != 1 ->
    // None`), on the reasoning that a browser holds a single replica so the
    // picker must choose. In practice that meant a browser node on a tenant with
    // two or more browser_db projects replicated NOTHING AT ALL, permanently and
    // silently — the common case, and the opposite of the intent.
    //
    // A single replica per browser is a real constraint, but it argues for
    // CHOOSING deterministically, not for declining. Rendezvous-hash the
    // candidate projects by the browser's own proven endpoint id, the same
    // FNV-over-sorted-set rule container placement and the inference coordinator
    // already use (`lease::hrw_owner`):
    //   * deterministic — every node computes the same answer for a given
    //     browser with no coordinator and no election;
    //   * stable — a browser keeps its project across renewals, so its OPFS
    //     replica stays warm instead of being re-seeded each lease tick;
    //   * self-sharding — distinct browsers hash to distinct projects, so N
    //     browser nodes spread across the tenant's databases rather than all
    //     piling onto whichever one happened to be alphabetically first;
    //   * minimal churn — removing a project moves only the browsers that held
    //     it, which is the property rendezvous hashing exists for.
    //
    // BTreeMap keys are already sorted, which is what makes the input set
    // order-independent across nodes.
    let projects: Vec<String> = by_project.keys().cloned().collect();
    let chosen = crate::lease::hrw_owner(endpoint_id, &projects)?;
    by_project.remove(&chosen).map(|(_, id)| id)
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
/// tenant-scoped live-status read (see [`project_db_status_http`]) and the
/// global-state shard planner's three operator reads (see [`shard_plan`]).
///
/// Round-robin-reads declaration (AGENTS.md) for the shard routes: every one
/// of them is a pure function of state that is ALREADY replicated to every
/// node — `store_sync::REGISTRY` snapshots and the `browser_presence` /
/// `browser_admissions` stores — so there is no node-local write for a GET to
/// miss and no owner to proxy to. Two nodes can disagree for one convergence
/// window (their store snapshots differ by a gossip round, so their fragment
/// digests differ); that is the same per-observer caveat as a health verdict,
/// and the `digest` guard on [`shard_fragment_http`] turns it into a loud 409
/// instead of a silently wrong byte range.
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
        .route(
            "/v1/browser-db/projects",
            axum::routing::get(browser_db_projects_http),
        )
        .route("/v1/browser/shards", axum::routing::get(shard_plan_http))
        .route(
            "/v1/browser/shards/verify",
            axum::routing::post(shard_verify_http),
        )
        .route(
            "/v1/browser/shards/fragment/:store/:index",
            axum::routing::get(shard_fragment_http),
        )
}

/// `GET /v1/browser-db/projects` — every project in the CALLER'S tenant that
/// carries a `browser_db` opt-in, with its raw policy, in ONE request.
///
/// The Storage page renders one list from two backends, and the managed half
/// is a single endpoint that cannot partially fail. The SQLite half had no
/// equivalent, so the page fanned out one `/v1/projects/<p>/settings` request
/// per project and assembled the lane client-side — which made a database's
/// presence in the list depend on N independent requests all succeeding, and
/// promptly. Witnessed: the same tenant's SQLite row present in one load and
/// absent from the next. This is that missing endpoint; the two halves of one
/// list now have the same failure mode.
///
/// Tenant-filtered per project through the same `project_owned_by` guard the
/// per-project route uses, so this can only ever widen to what the caller
/// already owns — a project belonging to another team is simply omitted, which
/// is also why an unknown project and a foreign one are indistinguishable here.
async fn browser_db_projects_http(
    axum::extract::State(cloud): axum::extract::State<Arc<CloudState>>,
    headers: axum::http::HeaderMap,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, String)> {
    let tenant = crate::admin::tenant(&cloud, &headers, claims.as_ref().map(|e| &e.0));
    let mut owned: Vec<(String, BrowserDbPolicy)> = cloud
        .projects
        .browser_db_projects()
        .into_iter()
        .filter(|(project, _)| crate::admin::project_owned_by(&cloud, project, &tenant))
        .collect();
    // Deterministic order so a re-poll never reshuffles the rendered list.
    owned.sort_by(|a, b| a.0.cmp(&b.0));
    let projects: Vec<Value> = owned
        .into_iter()
        .map(|(project, policy)| json!({ "project": project, "browser_db": policy }))
        .collect();
    Ok(axum::Json(json!({ "projects": projects })))
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
            reconcile_shards(&cloud);
            tick.tick().await;
        }
    });
}

// ---------------------------------------------------------------------------
// Fleet <-> fleet anti-entropy (direct, no browser carrier)
// ---------------------------------------------------------------------------
//
// v1 converged FLEET replicas only through browser CARRIERS: a Team browser
// that pulled node A's changes later pushed them to node B. That satisfies the
// browser-facing contract, but it makes the fleet replica set — which the
// contract calls the system of record — depend on somebody's tab being open.
// Two consequences, both real:
//
//   * a project whose browsers are all closed stops converging between nodes,
//     silently, for as long as that lasts; and
//   * `reconcile_replicas` creates an EMPTY replica on a node that never held
//     one, and with no fleet-side source that emptiness is only ever filled by
//     a browser. A node that just joined therefore answers for a database it
//     holds nothing of — which is exactly what makes an owner-pinned SQL
//     endpoint unsafe to build on top of the browser-carrier model.
//
// This is the direct arm the contract names as the deliberate follow-up. It is
// PULL-ONLY and reuses the exchange verbatim: same `Op::CrrSync` encodings,
// same `sync_round` responder, same caps, same whole-batch rollback. Pull-only
// is what keeps it reasonable — each node periodically asks a peer "what do you
// have that I do not", which is textbook anti-entropy and needs no write-
// permission model beyond the peer authentication the mesh already performs.
// It is also strictly additive: a peer running a pre-upgrade binary has no
// dispatch arm for the path and simply fails the round, which is the same
// no-op as an unreachable peer.

const DEFAULT_FLEET_SYNC_SECS: u64 = 45;
const FLEET_SYNC_TIMEOUT_SECS: u64 = 20;
/// Peers pulled per project per tick. Bounded so the round cost stays O(1) in
/// fleet size per tick while the ROTOR still reaches every peer over time.
const FLEET_SYNC_PEERS_PER_TICK: usize = 2;

/// The grant a FLEET peer syncs under. Peers are node-authenticated by the mesh
/// transport and hold the same system-of-record role this node does, so the
/// grant is read+write over any project this node knows to be opted in. An
/// unknown project yields `None` — the identical refusal shape the browser path
/// uses, so a caller learns nothing about what exists here.
fn fleet_round_grant(cloud: &Arc<CloudState>, project: &str) -> Option<RoundGrant> {
    let spec = opted_in_projects(cloud).remove(project)?;
    Some(RoundGrant {
        project: project.to_string(),
        resolved: spec.resolve(),
        read_only: false,
    })
}

/// Responder half, dispatched from `gossip::dispatch`'s
/// `/v1/browser-db/mesh-sync/` POST arm. Request and reply are the SAME
/// `Op::CrrSync` encodings the browser lane uses, so one wire format and one
/// `sync_round` serve both callers. An empty reply is the refusal; the dialer
/// treats empty/malformed as a failed round and simply retries next tick.
pub async fn mesh_crr_sync(cloud: &Arc<CloudState>, project: &str, body: &[u8]) -> Vec<u8> {
    let Some(grant) = fleet_round_grant(cloud, project) else {
        tracing::debug!(%project, "fleet db sync refused: project is not opted in on this node");
        return Vec::new();
    };
    let Ok(request) = hive_browser_proto::split_crr_sync_request(body) else {
        tracing::warn!(%project, "fleet db sync: malformed request");
        return Vec::new();
    };
    // Same rule as the browser path: `db_file` is a grant IDENTIFIER, never a
    // path, and must equal the name THIS node derives from its own grant.
    if request.db_file != replica_file_name(&grant.project) {
        tracing::warn!(%project, "fleet db sync refused: db_file does not match the derived name");
        return Vec::new();
    }
    let path = store_dir().join(replica_file_name(&grant.project));
    let label = grant.project.clone();
    match tokio::task::spawn_blocking(move || sync_round(&path, &grant, &request)).await {
        Ok(Ok(reply)) => hive_browser_proto::encode_crr_sync_reply(&reply),
        Ok(Err(code)) => {
            tracing::warn!(project = %label, code, "fleet db sync round failed");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(project = %label, error = %e, "fleet db sync worker join failed");
            Vec::new()
        }
    }
}

/// One PULL round against `peer`: advertise this node's per-site watermarks
/// with an empty push, then apply whatever the peer exports that this node is
/// missing. Returns the number of batches applied.
async fn fleet_pull_round(
    cloud: &Arc<CloudState>,
    project: &str,
    peer_id: &str,
    addr: &str,
) -> Result<usize, String> {
    let grant = fleet_round_grant(cloud, project).ok_or("project is not opted in here")?;
    let path = store_dir().join(replica_file_name(&grant.project));
    let file = replica_file_name(&grant.project);

    let wm_path = path.clone();
    let watermarks = tokio::task::spawn_blocking(move || -> Result<Vec<(Vec<u8>, i64)>, String> {
        if let Some(parent) = wm_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let conn = hive_crsql::open(&wm_path).map_err(|e| e.to_string())?;
        hive_crsql::known_sites(&conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    let request = hive_browser_proto::encode_crr_sync_request(&hive_browser_proto::CrrSyncRequest {
        db_file: file,
        push_more: false,
        watermarks,
        batches: Vec::new(),
    });

    let reply_bytes = crate::gossip::request_to(
        cloud,
        peer_id,
        addr,
        hive_p2p::GOSSIP_POST,
        &format!("/v1/browser-db/mesh-sync/{project}"),
        &request,
        FLEET_SYNC_TIMEOUT_SECS,
    )
    .await
    .ok_or("peer unreachable, or it refused the round")?;

    if reply_bytes.is_empty() {
        return Ok(0);
    }
    let reply = hive_browser_proto::split_crr_sync_reply(&reply_bytes).map_err(|e| e.to_string())?;
    if reply.batches.is_empty() {
        return Ok(0);
    }
    let applied = reply.batches.len();
    let resolved = grant.resolved.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let conn = hive_crsql::open(&path).map_err(|e| e.to_string())?;
        ensure_schema(&conn, &resolved).map_err(|e| e.to_string())?;
        let (status, message) = apply_push_batches(&conn, &path, &resolved, &reply.batches);
        if !matches!(status, CrrStatus::Ok) {
            // Typed refusals (quota, oversized value) are the SAME rollback the
            // browser path takes — never a partial apply.
            return Err(message);
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(applied)
}

/// Registered in `main.rs` beside [`spawn_reconcile`]. Every node runs it: the
/// replica is per-host, so this is peer-to-peer anti-entropy, not a leader job.
/// `HIVE_BROWSER_DB_FLEET_SYNC=0` disables the DIALING half (the responder arm
/// stays, so a disabled node still serves peers) — the `HIVE_BROWSER_DB_LISTEN`
/// precedent for having an ops-side off switch that degrades to a no-op.
pub fn spawn_fleet_sync(cloud: Arc<CloudState>) {
    if std::env::var("HIVE_BROWSER_DB_FLEET_SYNC")
        .map(|v| v.trim() == "0")
        .unwrap_or(false)
    {
        tracing::info!("browser db fleet-fleet sync disabled (HIVE_BROWSER_DB_FLEET_SYNC=0)");
        return;
    }
    let interval_secs = env_u64("HIVE_BROWSER_DB_FLEET_SYNC_SECS", DEFAULT_FLEET_SYNC_SECS);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        tick.tick().await;
        tick.tick().await;
        // Rotates so the bounded per-tick peer sample still covers the whole
        // fleet over successive ticks instead of hammering the same two peers.
        let mut rotor: usize = 0;
        loop {
            let projects: Vec<String> = opted_in_projects(&cloud).into_keys().collect();
            let peers: Vec<(String, String, String)> = cloud
                .registry
                .nodes()
                .iter()
                .filter(|n| n.healthy && n.name != cloud.node_name)
                .filter_map(|n| Some((n.name.clone(), n.peer_id.clone()?, n.iroh_addr.clone()?)))
                .collect();
            if !projects.is_empty() && !peers.is_empty() {
                for project in &projects {
                    for k in 0..FLEET_SYNC_PEERS_PER_TICK.min(peers.len()) {
                        let (name, peer_id, addr) = &peers[(rotor + k) % peers.len()];
                        match fleet_pull_round(&cloud, project, peer_id, addr).await {
                            Ok(0) => {}
                            Ok(n) => tracing::info!(
                                %project, peer = %name, batches = n,
                                "fleet db sync applied peer changes"
                            ),
                            Err(e) => tracing::debug!(
                                %project, peer = %name, error = %e, "fleet db sync round failed"
                            ),
                        }
                    }
                }
                rotor = rotor.wrapping_add(FLEET_SYNC_PEERS_PER_TICK);
            }
            tick.tick().await;
        }
    });
}

// ---------------------------------------------------------------------------
// Global-state shards (bn-browser-state-shards)
// ---------------------------------------------------------------------------
//
// A browser cannot hold the platform's replicated state — so it holds SMALL
// FRAGMENTS of it, and which fragments is a question every node answers
// identically with no coordinator.
//
// The pieces, in the order they compose:
//
//  1. FRAGMENTS. Every entry in `store_sync::REGISTRY` already produces
//     CANONICAL, deterministic bytes for equal state (that is the contract
//     the follower's byte-compare change-gate depends on). Those bytes are
//     the global CRDT/GuardianDB state as this platform actually stores it,
//     so they are what gets fragmented: each store's snapshot is cut into
//     `fragment_bytes`-sized pieces on UTF-8 boundaries, and each piece is
//     content-addressed by BLAKE3 (`fluid_core::browser_source_digest`, the
//     same digest the browser-artifact pins use — literally
//     `blake3::hash(bytes)`).
//
//  2. ASSIGNMENT. Rendezvous/HRW hashing over the membership set, reusing
//     `lease::hrw_owner` — the SAME function container placement and the
//     inference coordinator election already agree through, called
//     repeatedly against a shrinking pool to get a ranked top-R rather than a
//     single owner. Placement keys on the fragment KEY (`<store>/<index>`),
//     never on its digest, so a state change does not move fragments and a
//     membership change moves only the fragments the departed peer held.
//
//  3. BOUND. A hard per-browser byte cap, defaulted SMALL (4 MiB). A
//     fragment that would push a holder over its cap is a TYPED REFUSAL
//     naming the holder and the fragment — never a truncated fragment, never
//     a partial one, and never silently re-homed onto a peer HRW did not
//     rank (which would trade a visible shortfall for an invisible
//     order-dependent placement). The shortfall surfaces honestly as
//     `under_replicated_fragments`. This is `apply_push_batches`' cap
//     discipline — typed refusal, whole unit rolled back, loud message —
//     applied to placement instead of to a write.
//
//  4. RE-DERIVATION. `shard_plan` holds no state at all: it is recomputed
//     from the live membership every call, so a join/leave/expiry is picked
//     up by construction. `membership_digest` exists so a caller can tell
//     cheaply that the answer it is holding is stale, and
//     [`reconcile_shards`] logs the transition.
//
// TRUST — what is implemented and what is NOT. See [`shard_trust_json`],
// which returns this same statement to every API caller so it cannot drift
// away from the code:
//
//  * IMPLEMENTED: content-addressed fragments. A fragment's identity IS the
//    BLAKE3 of its bytes, and [`verify_fragment`] re-checks exact length and
//    digest before any byte is used. A browser that returns WRONG bytes is
//    therefore caught deterministically, without trusting the browser at all.
//
//  * NOT IMPLEMENTED, AND NOT CLAIMED: this proves nothing about continued
//    possession. A browser that stores nothing, discards a fragment the
//    moment it is assigned, or serves it back by re-fetching it from another
//    node, is indistinguishable from one honestly holding it — until the
//    moment it is asked and fails. Content addressing bounds CORRUPTION, not
//    AVAILABILITY. It also gives no confidentiality: a fragment handed to a
//    browser is readable by that browser.
//
//  * WHY THE BAR IS PLATFORM-ADMIN TODAY. Because of the two gaps above,
//    eligibility is gated on `browser_presence`'s server-derived
//    `shard_eligible`, set only from a `platform_admin` session. A platform
//    admin can already read this state through the operator console, so
//    handing them a fragment of it discloses nothing new. Opening this to
//    non-admin donors needs BOTH halves the current design lacks:
//    confidentiality (encrypt fragments at rest on the donor, so a fragment
//    is opaque to its holder) and a real possession argument.
//
//  * THE HONEST OPTIONS for that second half, none of them implemented here,
//    none to be hand-rolled: (a) accept the weaker guarantee and rely on
//    over-replication plus audit challenges — cheap, standard, and it only
//    detects loss after the fact; (b) proof-of-retrievability / provable
//    data possession (Juels–Kaliski PoR, Ateniese PDP, or a Merkle-tree
//    challenge–response over erasure-coded fragments), which is what
//    actually proves continued possession and is a scheme to ADOPT from the
//    literature with its published parameters, never to invent; (c) do not
//    ask browsers to prove anything and treat every browser replica as a
//    cache with replication factor zero — exactly the posture
//    `docs/browser-db-contract.md` already takes for browser DB replicas.
//    (c) is the cheapest correct answer and the current default.

/// Per-browser hard ceiling on held fragment bytes. Deliberately SMALL: a
/// donor's browser tab is not storage, and the whole premise is fragments.
const DEFAULT_SHARD_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Even an operator typo must not hand a browser tab a gigabyte.
const SHARD_MAX_BYTES_CEILING: u64 = 64 * 1024 * 1024;
const DEFAULT_SHARD_FRAGMENT_BYTES: u64 = 64 * 1024;
const SHARD_FRAGMENT_BYTES_MIN: u64 = 4 * 1024;
const SHARD_FRAGMENT_BYTES_MAX: u64 = 1024 * 1024;
const DEFAULT_SHARD_REPLICATION: u64 = 2;
const SHARD_REPLICATION_MAX: u64 = 8;
/// Enumeration bound. Reaching it is a typed refusal naming the store it
/// stopped in, never a silent short table.
const SHARD_MAX_TOTAL_FRAGMENTS: usize = 100_000;
/// Refusals are reported bounded + counted, never elided: `refusals_total`
/// always carries the true number.
const SHARD_MAX_REPORTED_REFUSALS: usize = 64;

/// One content-addressed fragment of one replicated store's canonical
/// snapshot. `key` is the PLACEMENT identity (stable across content changes);
/// `digest` is the CONTENT identity (changes with every write).
#[derive(Clone, Debug, Serialize)]
pub struct StateFragment {
    /// `<store>/<zero-padded index>` — zero-padded so lexical order over keys
    /// is numeric order over fragments, which is what makes the cap
    /// accounting below deterministic on every node.
    pub key: String,
    pub store: String,
    pub index: u32,
    pub bytes: u64,
    /// BLAKE3, lowercase hex, of exactly these bytes.
    pub digest: String,
}

/// A refusal the plan is REPORTING, not hiding. Every one names the fragment,
/// the holder it applies to (empty when it applies to no holder) and why.
#[derive(Clone, Debug, Serialize)]
pub struct ShardRefusal {
    pub reason: &'static str,
    pub fragment: String,
    pub endpoint_id: String,
    pub detail: String,
}

/// What one browser peer is responsible for. `fragments` is bounded by
/// construction: `max_bytes / fragment_bytes` entries at most (64 at the
/// defaults), so this is always a renderable list, never a firehose.
#[derive(Clone, Debug, Serialize)]
pub struct ShardHolding {
    pub endpoint_id: String,
    /// `browser_presence::node_name` — the same `bn-…` identity the
    /// constellation renders.
    pub node_name: String,
    pub max_bytes: u64,
    pub held_bytes: u64,
    pub fragments: Vec<StateFragment>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShardParams {
    pub enabled: bool,
    pub max_bytes: u64,
    pub fragment_bytes: u64,
    pub replication_factor: usize,
    /// The replicated stores actually being fragmented, sorted.
    pub stores: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShardPlan {
    pub params: ShardParams,
    /// Eligible browser endpoint ids, sorted — the HRW membership set.
    pub membership: Vec<String>,
    /// Digest of the membership set. A caller holding a plan compares this to
    /// know its assignment is stale without re-reading the whole plan.
    pub membership_digest: String,
    /// Commits to the membership AND to every fragment digest, so it also
    /// changes when the underlying state moves.
    pub plan_digest: String,
    pub fragments_total: usize,
    pub fragment_bytes_total: u64,
    pub replicas_wanted: usize,
    pub replicas_placed: usize,
    /// Fragments placed on at least one but fewer than `replication_factor`
    /// holders.
    pub under_replicated_fragments: usize,
    /// Fragments placed on NO holder at all.
    pub unplaced_fragments: usize,
    pub holdings: Vec<ShardHolding>,
    pub refusals: Vec<ShardRefusal>,
    pub refusals_total: usize,
}

fn env_u64_clamped(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
        .clamp(min, max)
}

/// `HIVE_BROWSER_SHARDS=0` turns the planner off entirely (the
/// `HIVE_BROWSER_DB_LISTEN=0` precedent): the plan still renders, with an
/// empty membership and `enabled: false`, so an operator sees WHY there are
/// no assignments rather than an ambiguous empty list.
fn shard_params() -> ShardParams {
    let enabled = std::env::var("HIVE_BROWSER_SHARDS")
        .map(|v| !matches!(v.trim(), "0" | "false" | "off"))
        .unwrap_or(true);
    let selected: BTreeSet<String> = std::env::var("HIVE_BROWSER_SHARD_STORES")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    // REGISTRY order is a source-code fact and therefore already identical on
    // identical builds; sorting makes it identical across a MIXED-version
    // fleet too, which is exactly when a placement disagreement would be
    // hardest to see.
    let mut stores: Vec<String> = crate::store_sync::REGISTRY
        .iter()
        .map(|s| s.name.to_string())
        .filter(|name| selected.is_empty() || selected.contains(name))
        .collect();
    stores.sort();
    ShardParams {
        enabled,
        max_bytes: env_u64_clamped(
            "HIVE_BROWSER_SHARD_MAX_BYTES",
            DEFAULT_SHARD_MAX_BYTES,
            1,
            SHARD_MAX_BYTES_CEILING,
        ),
        fragment_bytes: env_u64_clamped(
            "HIVE_BROWSER_SHARD_FRAGMENT_BYTES",
            DEFAULT_SHARD_FRAGMENT_BYTES,
            SHARD_FRAGMENT_BYTES_MIN,
            SHARD_FRAGMENT_BYTES_MAX,
        ),
        replication_factor: env_u64_clamped(
            "HIVE_BROWSER_SHARD_REPLICATION",
            DEFAULT_SHARD_REPLICATION,
            1,
            SHARD_REPLICATION_MAX,
        ) as usize,
        stores,
    }
}

/// The eligible membership set: browser peers that are `shard_eligible`
/// (server-stamped from a `platform_admin` session), currently `online`, and
/// STILL carrying a live admission on this node's own replicated view.
///
/// The admission re-check is not redundant with presence. Presence is issued
/// alongside an admission and torn down with it
/// (`browser_presence::remove_for_endpoint`), but the two stores replicate
/// independently and a presence record can outlive a revocation by one gossip
/// round — the `proxy_to_owner` / `resolve_round_grant` re-check precedent:
/// authorization is re-derived at the point of use, from proven identity,
/// every time.
pub fn shard_members(cloud: &Arc<CloudState>) -> Vec<(String, String)> {
    let now = hive_core::now_ms();
    cloud
        .browser_presence
        .shard_candidates(now)
        .into_iter()
        .filter(|record| {
            cloud
                .browser_admissions
                .live_for_endpoint(&record.endpoint_id, now)
                .is_some()
        })
        .map(|record| (record.endpoint_id, record.node_name))
        .collect()
}

/// Ranked rendezvous hashing: the top `take` preferred holders of `key`.
///
/// Built ONLY out of `lease::hrw_owner` — the same weight function container
/// placement and the inference-coordinator election agree through — applied
/// to a shrinking pool. Removing the winner and re-running is exactly HRW
/// rank order, so the top-1 here is by construction the same node those
/// callers would pick, and the minimal-churn property carries: a member
/// leaving promotes each fragment's next-ranked holder and moves nothing
/// else.
fn hrw_rank(key: &str, members: &[String], take: usize) -> Vec<String> {
    let mut pool: Vec<String> = members.to_vec();
    let mut ranked: Vec<String> = Vec::with_capacity(take.min(pool.len()));
    while ranked.len() < take {
        let Some(top) = crate::lease::hrw_owner(key, &pool) else {
            break;
        };
        pool.retain(|member| *member != top);
        ranked.push(top);
    }
    ranked
}

/// Cut `text` into `target`-byte pieces that never split a UTF-8 character.
///
/// The walk-back can move at most 3 bytes and `target` is floored at 4 KiB,
/// so a piece can never be empty and the loop always advances — no zero-length
/// fragment, no spin.
fn utf8_chunks(text: &str, target: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = (start + target).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        out.push((start, end));
        start = end;
    }
    out
}

/// Enumerate every fragment of every selected store, in stable
/// `(store, index)` order, alongside any store-level refusals.
///
/// Fragment CONTENT is not retained here — only its descriptor. The bytes are
/// re-derived on demand by [`shard_fragment_http`], which is why the plan can
/// be recomputed on every call without holding a copy of platform state.
fn fragment_table(
    cloud: &Arc<CloudState>,
    params: &ShardParams,
) -> (Vec<StateFragment>, Vec<ShardRefusal>) {
    let mut fragments = Vec::new();
    let mut refusals = Vec::new();
    for store in &params.stores {
        let bytes = crate::store_sync::serve(cloud, store);
        if bytes.is_empty() {
            continue;
        }
        // Every REGISTRY snapshot is serde_json output and therefore UTF-8;
        // this arm exists so a future non-JSON store is named loudly instead
        // of silently vanishing from the shard set.
        let Ok(text) = std::str::from_utf8(&bytes) else {
            refusals.push(ShardRefusal {
                reason: "store_not_utf8",
                fragment: store.clone(),
                endpoint_id: String::new(),
                detail: format!(
                    "store {store} snapshot ({} bytes) is not UTF-8 and cannot be fragmented",
                    bytes.len()
                ),
            });
            continue;
        };
        for (index, (start, end)) in utf8_chunks(text, params.fragment_bytes as usize)
            .into_iter()
            .enumerate()
        {
            if fragments.len() >= SHARD_MAX_TOTAL_FRAGMENTS {
                refusals.push(ShardRefusal {
                    reason: "fragment_budget_exhausted",
                    fragment: format!("{store}/{index:05}"),
                    endpoint_id: String::new(),
                    detail: format!(
                        "stopped enumerating at {SHARD_MAX_TOTAL_FRAGMENTS} fragments inside store {store}; raise HIVE_BROWSER_SHARD_FRAGMENT_BYTES or narrow HIVE_BROWSER_SHARD_STORES"
                    ),
                });
                return (fragments, refusals);
            }
            let piece = &text[start..end];
            fragments.push(StateFragment {
                key: format!("{store}/{index:05}"),
                store: store.clone(),
                index: index as u32,
                bytes: piece.len() as u64,
                digest: fluid_core::browser_source_digest(piece),
            });
        }
    }
    (fragments, refusals)
}

/// The whole assignment, recomputed from live state on every call.
///
/// Determinism: fragments are walked in lexical key order and members are
/// sorted, so the cap accounting below visits identical inputs in an
/// identical order on every node — the plan is a pure function of (fragment
/// table, membership, params) with no clock, no counter and no local history
/// in it.
pub fn shard_plan(cloud: &Arc<CloudState>) -> ShardPlan {
    let params = shard_params();
    let (fragments, mut refusals) = fragment_table(cloud, &params);
    let fragment_bytes_total = fragments.iter().map(|f| f.bytes).sum();

    let members: Vec<(String, String)> = if params.enabled {
        shard_members(cloud)
    } else {
        Vec::new()
    };
    let ids: Vec<String> = members.iter().map(|(id, _)| id.clone()).collect();
    let membership_digest = fluid_core::browser_source_digest(&ids.join("\n"));
    let plan_digest = {
        let mut commit = String::with_capacity(64 + fragments.len() * 65);
        commit.push_str(&membership_digest);
        commit.push('\u{0}');
        commit.push_str(&format!(
            "{}:{}:{}",
            params.max_bytes, params.fragment_bytes, params.replication_factor
        ));
        for fragment in &fragments {
            commit.push('\n');
            commit.push_str(&fragment.digest);
        }
        fluid_core::browser_source_digest(&commit)
    };

    let mut holdings: BTreeMap<String, ShardHolding> = members
        .iter()
        .map(|(id, name)| {
            (
                id.clone(),
                ShardHolding {
                    endpoint_id: id.clone(),
                    node_name: name.clone(),
                    max_bytes: params.max_bytes,
                    held_bytes: 0,
                    fragments: Vec::new(),
                },
            )
        })
        .collect();

    let mut replicas_placed = 0usize;
    let mut under_replicated = 0usize;
    let mut unplaced = 0usize;
    for fragment in &fragments {
        // A fragment bigger than the per-browser cap can never be placed
        // ANYWHERE — the same shape as browser_db's "a batch that cannot fit
        // any frame stays local, named". Splitting it further to make it fit
        // would silently change the fragment identity every consumer pins.
        if fragment.bytes > params.max_bytes {
            unplaced += 1;
            refusals.push(ShardRefusal {
                reason: "fragment_exceeds_cap",
                fragment: fragment.key.clone(),
                endpoint_id: String::new(),
                detail: format!(
                    "fragment is {} bytes; no browser may hold more than max_bytes {}",
                    fragment.bytes, params.max_bytes
                ),
            });
            continue;
        }
        let ranked = hrw_rank(&fragment.key, &ids, params.replication_factor);
        if ranked.is_empty() {
            unplaced += 1;
            refusals.push(ShardRefusal {
                reason: "no_eligible_member",
                fragment: fragment.key.clone(),
                endpoint_id: String::new(),
                detail: if params.enabled {
                    "no online, shard-eligible browser peer with a live admission".to_string()
                } else {
                    "shard planning is disabled (HIVE_BROWSER_SHARDS=0)".to_string()
                },
            });
            continue;
        }
        let mut placed = 0usize;
        for holder in &ranked {
            let Some(holding) = holdings.get_mut(holder) else {
                continue;
            };
            // Cap check BEFORE the assignment, whole fragment or nothing.
            // Note what does NOT happen on refusal: the fragment is not
            // trimmed to fit, and it is not re-homed onto a member outside
            // its HRW top-R. Re-homing would make placement depend on
            // iteration order and destroy the minimal-churn property; the
            // shortfall is reported instead.
            if holding.held_bytes.saturating_add(fragment.bytes) > holding.max_bytes {
                refusals.push(ShardRefusal {
                    reason: "browser_cap_exceeded",
                    fragment: fragment.key.clone(),
                    endpoint_id: holder.clone(),
                    detail: format!(
                        "{} holds {} bytes; a {}-byte fragment would exceed max_bytes {}",
                        holding.node_name, holding.held_bytes, fragment.bytes, holding.max_bytes
                    ),
                });
                continue;
            }
            holding.held_bytes += fragment.bytes;
            holding.fragments.push(fragment.clone());
            placed += 1;
        }
        replicas_placed += placed;
        if placed == 0 {
            unplaced += 1;
        } else if placed < params.replication_factor {
            under_replicated += 1;
        }
    }

    // Bounded in what is REPORTED, never in what is COUNTED: `refusals_total`
    // is taken before the truncation, so a plan with 900 refusals says 900 and
    // shows 64 rather than quietly looking like a plan with 64.
    let refusals_total = refusals.len();
    refusals.truncate(SHARD_MAX_REPORTED_REFUSALS);
    ShardPlan {
        replicas_wanted: fragments.len() * params.replication_factor,
        fragments_total: fragments.len(),
        fragment_bytes_total,
        replicas_placed,
        under_replicated_fragments: under_replicated,
        unplaced_fragments: unplaced,
        holdings: holdings.into_values().collect(),
        refusals,
        refusals_total,
        membership: ids,
        membership_digest,
        plan_digest,
        params,
    }
}

/// The gate every fragment byte must pass before it is used — the one piece
/// of the trust story that is actually implemented.
///
/// Exact length first (a truncated body can never be "close enough"), then
/// UTF-8, then BLAKE3 against the descriptor's `digest`. A browser that
/// returns wrong bytes is caught here deterministically without trusting the
/// browser at all. What this does NOT do is establish that the browser was
/// holding the bytes rather than fetching them on demand, or that it will
/// still have them next time — see this section's TRUST notes.
pub fn verify_fragment(fragment: &StateFragment, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 != fragment.bytes {
        return Err(format!(
            "fragment {} is {} bytes, expected exactly {}",
            fragment.key,
            bytes.len(),
            fragment.bytes
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|e| format!("fragment {} is not valid UTF-8: {e}", fragment.key))?;
    let digest = fluid_core::browser_source_digest(text);
    if digest != fragment.digest {
        return Err(format!(
            "fragment {} digest mismatch: got {digest}, expected {}",
            fragment.key, fragment.digest
        ));
    }
    Ok(())
}

/// The trust statement, returned to every shard API caller so the claim
/// travels with the data and cannot quietly diverge from the implementation.
fn shard_trust_json() -> Value {
    json!({
        "model": "content-addressed fragments (BLAKE3) + server-derived platform-admin eligibility",
        "proves": [
            "a returned fragment is byte-exact: length and BLAKE3 digest are re-checked before any use (verify_fragment), so a holder cannot serve wrong or corrupted bytes undetected",
            "assignment is coordinator-free and identical on every node: HRW (lease::hrw_owner) over a server-derived membership set",
            "a browser cannot choose its own identity or its own eligibility — both are derived server-side from the proven endpoint id and the authenticated session"
        ],
        "does_not_prove": [
            "CONTINUED POSSESSION: nothing here shows a browser still holds a fragment, or ever did. A holder that discards a fragment, or re-fetches it from another node on demand, is indistinguishable from an honest one until it is asked and fails",
            "AVAILABILITY: content addressing bounds corruption, not loss",
            "CONFIDENTIALITY: a fragment handed to a browser is readable by that browser. This is why eligibility is platform-admin-only today — an admin can already read this state through the operator console"
        ],
        "options_for_a_real_possession_guarantee": [
            "accept the weaker guarantee: over-replicate and audit by random challenge; detects loss after the fact, costs nothing new",
            "adopt a published proof-of-retrievability / provable-data-possession scheme (Juels-Kaliski PoR, Ateniese PDP, or Merkle challenge-response over erasure-coded fragments) with its published parameters — adopt, never invent",
            "do not ask browsers to prove anything: treat every browser copy as a cache with replication factor zero, the posture docs/browser-db-contract.md already takes for browser DB replicas"
        ],
        "implemented_here": "assignment, bounding and verification only. There is no transport arm that fetches a fragment back from a browser, and no possession challenge."
    })
}

/// Re-derivation driver. The plan itself is stateless — it is recomputed from
/// live membership on every read, so nothing here has to *cause* the
/// re-derivation. This loop's job is purely to NOTICE, so a fleet that quietly
/// stopped being able to place fragments shows up in the log and not only in
/// an operator's browser.
///
/// The membership digest is computed FIRST, on its own, and the loop returns
/// on no change. That ordering is the whole point: [`shard_plan`] serializes
/// every replicated store to build its fragment table, and this runs on every
/// node every `HIVE_BROWSER_DB_RECONCILE_SECS`. Building a full plan per tick
/// just to compare one digest would spend that serialization on nothing in the
/// steady state, which is exactly the shape of cost that is invisible until a
/// store grows.
fn reconcile_shards(cloud: &Arc<CloudState>) {
    static LAST_MEMBERSHIP: parking_lot::Mutex<String> = parking_lot::Mutex::new(String::new());
    let members: Vec<String> = if shard_params().enabled {
        shard_members(cloud).into_iter().map(|(id, _)| id).collect()
    } else {
        Vec::new()
    };
    let digest = fluid_core::browser_source_digest(&members.join("\n"));
    {
        let mut last = LAST_MEMBERSHIP.lock();
        if *last == digest {
            return;
        }
        *last = digest.clone();
    }
    let plan = shard_plan(cloud);
    tracing::info!(
        members = plan.membership.len(),
        fragments = plan.fragments_total,
        replicas_placed = plan.replicas_placed,
        replicas_wanted = plan.replicas_wanted,
        membership_digest = %plan.membership_digest,
        "browser shards: membership changed, fragment assignment re-derived"
    );
    if plan.unplaced_fragments > 0 || plan.under_replicated_fragments > 0 {
        tracing::warn!(
            unplaced = plan.unplaced_fragments,
            under_replicated = plan.under_replicated_fragments,
            refusals = plan.refusals_total,
            first_refusal = plan.refusals.first().map(|r| r.detail.as_str()).unwrap_or(""),
            "browser shards: fragments are not fully placed"
        );
    }
}

/// `GET /v1/browser/shards` — operator-only. The whole plan plus the trust
/// statement.
async fn shard_plan_http(
    axum::extract::State(cloud): axum::extract::State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, String)> {
    crate::admin::require_operator(claims.map(|c| c.0).as_ref())?;
    let plan = shard_plan(&cloud);
    Ok(axum::Json(json!({ "plan": plan, "trust": shard_trust_json() })))
}

/// `GET /v1/browser/shards/fragment/:store/:index[?digest=<hex>]` —
/// operator-only. The fragment's actual bytes, re-derived from this node's
/// own copy of the store.
///
/// `store` is matched against `store_sync::REGISTRY` by exact name and
/// `index` parses as an integer, so neither path segment can become a file
/// path or a query — the same "validate before it is ever a path component"
/// rule `browser_artifacts` applies to a digest.
///
/// The optional `digest` is the anti-skew guard: state moves, and two nodes
/// can be one gossip round apart. Passing the digest the plan advertised
/// turns "this node's bytes have since changed" into a loud 409 instead of a
/// body that silently fails the caller's own verification later.
async fn shard_fragment_http(
    axum::extract::State(cloud): axum::extract::State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::extract::Path((store, index)): axum::extract::Path<(String, u32)>,
    axum::extract::Query(query): axum::extract::Query<BTreeMap<String, String>>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, String)> {
    crate::admin::require_operator(claims.map(|c| c.0).as_ref())?;
    let params = shard_params();
    if !params.stores.iter().any(|s| *s == store) {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "unknown or unsharded store".to_string(),
        ));
    }
    let bytes = crate::store_sync::serve(&cloud, &store);
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        (
            axum::http::StatusCode::CONFLICT,
            "store snapshot is not UTF-8 and is not fragmented".to_string(),
        )
    })?;
    let chunks = utf8_chunks(text, params.fragment_bytes as usize);
    let (start, end) = *chunks.get(index as usize).ok_or((
        axum::http::StatusCode::NOT_FOUND,
        "fragment index is past the end of this store".to_string(),
    ))?;
    let piece = &text[start..end];
    let digest = fluid_core::browser_source_digest(piece);
    if let Some(expected) = query.get("digest") {
        if *expected != digest {
            return Err((
                axum::http::StatusCode::CONFLICT,
                format!(
                    "fragment {store}/{index:05} is now {digest}, not {expected} — this node's state has moved; re-read the plan"
                ),
            ));
        }
    }
    Ok(axum::Json(json!({
        "key": format!("{store}/{index:05}"),
        "store": store,
        "index": index,
        "bytes": piece.len(),
        "digest": digest,
        "content": piece,
    })))
}

#[derive(Deserialize)]
struct ShardVerifyRequest {
    store: String,
    index: u32,
    /// The bytes being verified — a browser's answer, or an operator's
    /// deliberately corrupted copy of one.
    content: String,
    /// Optional: verify against THIS digest rather than the one this node
    /// currently derives, so a fragment captured earlier can still be checked
    /// after local state has moved.
    #[serde(default)]
    digest: Option<String>,
}

/// `POST /v1/browser/shards/verify` — operator-only. Runs
/// [`verify_fragment`] against caller-supplied bytes.
///
/// This is the verification GATE, exposed so the one property that is
/// actually proved can be exercised end-to-end today: post a fragment's real
/// content and it verifies; flip one character and it fails with the exact
/// digest mismatch. It stands in for the retrieval path deliberately — there
/// is no arm that fetches a fragment back from a browser yet, and pretending
/// otherwise is exactly the overclaim this module refuses to make.
async fn shard_verify_http(
    axum::extract::State(cloud): axum::extract::State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    axum::Json(request): axum::Json<ShardVerifyRequest>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, String)> {
    crate::admin::require_operator(claims.map(|c| c.0).as_ref())?;
    let params = shard_params();
    if !params.stores.iter().any(|s| *s == request.store) {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "unknown or unsharded store".to_string(),
        ));
    }
    // The descriptor to verify AGAINST is derived here, from this node's own
    // state (or from the caller's pinned digest) — never from the submitted
    // body's own claim about itself, which would make verification circular.
    let bytes = crate::store_sync::serve(&cloud, &request.store);
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        (
            axum::http::StatusCode::CONFLICT,
            "store snapshot is not UTF-8 and is not fragmented".to_string(),
        )
    })?;
    let chunks = utf8_chunks(text, params.fragment_bytes as usize);
    let (start, end) = *chunks.get(request.index as usize).ok_or((
        axum::http::StatusCode::NOT_FOUND,
        "fragment index is past the end of this store".to_string(),
    ))?;
    let piece = &text[start..end];
    let local_digest = fluid_core::browser_source_digest(piece);
    let (expected_digest, expected_bytes) = match &request.digest {
        // Verifying against a pinned digest: the length is the SUBMITTED
        // length, so a digest match is the whole proof. Against local state,
        // the local length is an independent second check.
        Some(pinned) => (pinned.clone(), request.content.len() as u64),
        None => (local_digest.clone(), piece.len() as u64),
    };
    if !hive_browser_proto::valid_blake3_digest(&expected_digest) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "digest must be 64 lowercase hex characters".to_string(),
        ));
    }
    let fragment = StateFragment {
        key: format!("{}/{:05}", request.store, request.index),
        store: request.store.clone(),
        index: request.index,
        bytes: expected_bytes,
        digest: expected_digest.clone(),
    };
    match verify_fragment(&fragment, request.content.as_bytes()) {
        Ok(()) => Ok(axum::Json(json!({
            "verified": true,
            "key": fragment.key,
            "digest": expected_digest,
            "matches_local_state": expected_digest == local_digest,
        }))),
        Err(error) => Ok(axum::Json(json!({
            "verified": false,
            "key": fragment.key,
            "digest": expected_digest,
            "local_digest": local_digest,
            "error": error,
        }))),
    }
}
