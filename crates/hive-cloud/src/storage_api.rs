//! Storage HTTP API — reflink/copy snapshots of a deployment's Firecracker
//! cell data image (`hive_backend::snapshot`), gated by `storage_broker`
//! (ownership + id safety + per-project quota).
//!
//! # NODE-LOCAL DATA — every route here declares which side it is on
//!
//! Per AGENTS.md's round-robin doc: a deployment's data image, and every
//! snapshot taken of it, live only on the ONE node that actually placed and
//! runs that cell — there is no replicated store and no leader for this data,
//! because placement itself is per-deployment, not fleet-uniform. So unlike a
//! sandbox mutation (which `sandboxes_api` forwards to the single
//! control-plane LEADER, because sandboxes have no independent placement),
//! a snapshot request has to reach whichever node actually hosts the
//! deployment — which may or may not be the leader, and may or may not be
//! the node the round-robin `api.<domain>` DNS happened to land this request
//! on.
//!
//! Every handler below therefore: (1) authorizes the caller against `project`
//! via `require_project` — a check that is itself fleet-aware and safe to run
//! on ANY node (see `admin::project_owned_by`'s own doc); (2) checks whether
//! THIS node's local gateway (`c.gw`) actually hosts `deployment_id`; (3) if
//! not, proxies the whole (already-authorized) request to the hosting node
//! via `admin::host_node_for_deployment` + an HTTP-then-mesh helper
//! (`post_body_to_host` / `fetch_from_host` / `delete_to_host`), mirroring
//! `deployment_build`'s identical fallback. The mesh side of that proxy has
//! its own `gossip::dispatch` arm (see gossip.rs) so a node with HTTP admin
//! dispatch blocked (a restrictive security group) still reaches the owning
//! node over iroh, per AGENTS.md's mesh-networking doc.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::admin::{
    delete_to_host, fetch_from_host, host_node_for_deployment, post_body_to_host, require_project,
};
use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route(
            "/v1/projects/:project/deployments/:deployment_id/snapshots",
            post(create_snapshot).get(list_snapshots),
        )
        .route(
            "/v1/projects/:project/deployments/:deployment_id/snapshots/:snapshot_id",
            delete(delete_snapshot),
        )
}

#[derive(Deserialize)]
pub(crate) struct CreateReq {
    pub(crate) snapshot_id: String,
    /// Whether the caller already quiesced the writer (checkpoint/flush/
    /// freeze) before calling — labels the outcome, never assumed.
    #[serde(default)]
    pub(crate) quiesce: bool,
}

/// A node running the mock/container backend has no per-cell data image at
/// all — `hive_backend::snapshot` is Firecracker-specific by construction
/// (see its own module doc on why: reflink needs a real block-backed ext4
/// image, which only the Firecracker cell path creates). Say so plainly
/// rather than silently no-op.
fn require_firecracker(
    c: &Arc<CloudState>,
) -> Result<Arc<hive_backend::firecracker::FirecrackerBackend>, (StatusCode, String)> {
    c.firecracker.clone().ok_or((
        StatusCode::NOT_IMPLEMENTED,
        "this node has no Firecracker cell backend — snapshots are only available for Firecracker-backed deployments".into(),
    ))
}

fn broker_status(e: crate::storage_broker::BrokerError) -> (StatusCode, String) {
    let code = match &e {
        crate::storage_broker::BrokerError::NotOwner => StatusCode::FORBIDDEN,
        crate::storage_broker::BrokerError::InvalidId(_) => StatusCode::BAD_REQUEST,
        crate::storage_broker::BrokerError::QuotaExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
    };
    (code, e.to_string())
}

/// Whether `deployment_id` is hosted on THIS node (this node's own gateway
/// placed and runs it) — the only case where the filesystem call below is
/// even meaningful.
fn hosted_here(c: &Arc<CloudState>, deployment_id: &str) -> bool {
    c.gw.deployment_records()
        .iter()
        .any(|r| r.id == deployment_id)
}

pub(crate) async fn create_snapshot(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, deployment_id)): Path<(String, String)>,
    Json(body): Json<CreateReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;

    if !hosted_here(&c, &deployment_id) {
        let Some(node) = host_node_for_deployment(&c, &deployment_id) else {
            return Err((StatusCode::NOT_FOUND, "unknown deployment".into()));
        };
        let path = format!("/v1/projects/{project}/deployments/{deployment_id}/snapshots");
        let body_v = json!({ "snapshot_id": body.snapshot_id, "quiesce": body.quiesce });
        return match post_body_to_host(&c, &node, &path, &t, &body_v).await {
            Some(v) => Ok(Json(v)),
            None => Err((
                StatusCode::BAD_GATEWAY,
                format!("storage node {node} unreachable"),
            )),
        };
    }

    let fc = require_firecracker(&c)?;
    let dir = fc.snapshot_dir(&deployment_id);
    let existing = hive_backend::snapshot::count_snapshots(&dir);
    crate::storage_broker::authorize_create(&c, &t, &project, &body.snapshot_id, existing)
        .map_err(broker_status)?;
    let Some(image) = fc.locate_data_image(&deployment_id) else {
        return Err((
            StatusCode::NOT_FOUND,
            "deployment has no data image on this node".into(),
        ));
    };
    let outcome =
        hive_backend::snapshot::snapshot_image(&image, &dir, &body.snapshot_id, body.quiesce)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(outcome)))
}

pub(crate) async fn list_snapshots(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, deployment_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;

    if !hosted_here(&c, &deployment_id) {
        if let Some(node) = host_node_for_deployment(&c, &deployment_id) {
            let path = format!("/v1/projects/{project}/deployments/{deployment_id}/snapshots");
            if let Some(v) = fetch_from_host(&c, &node, &path, &t).await {
                return Ok(Json(v));
            }
        }
        // Unknown/unreachable reads as "no snapshots visible from here", not
        // an error — matches every other node-local list endpoint's GET
        // posture (e.g. `list_sandboxes`'s empty-local fallback).
        return Ok(Json(json!({ "snapshots": [] })));
    }

    let fc = require_firecracker(&c)?;
    let dir = fc.snapshot_dir(&deployment_id);
    let mut ids = hive_backend::snapshot::list_snapshots(&dir);
    ids.sort();
    Ok(Json(json!({ "snapshots": ids })))
}

pub(crate) async fn delete_snapshot(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, deployment_id, snapshot_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)?;

    if !hosted_here(&c, &deployment_id) {
        let Some(node) = host_node_for_deployment(&c, &deployment_id) else {
            return Err((StatusCode::NOT_FOUND, "unknown deployment".into()));
        };
        let path =
            format!("/v1/projects/{project}/deployments/{deployment_id}/snapshots/{snapshot_id}");
        return match delete_to_host(&c, &node, &path, &t).await {
            Some(v) => Ok(Json(v)),
            None => Err((
                StatusCode::BAD_GATEWAY,
                format!("storage node {node} unreachable"),
            )),
        };
    }

    crate::storage_broker::authorize_delete(&c, &t, &project, &snapshot_id)
        .map_err(broker_status)?;
    let fc = require_firecracker(&c)?;
    let dir = fc.snapshot_dir(&deployment_id);
    let removed = hive_backend::snapshot::delete_snapshot(&dir, &snapshot_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "removed": removed })))
}
