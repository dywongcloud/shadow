//! shadw drive v1 — `/v1/drive/*` REST surface (bn-shadw-drive-v1).
//!
//! Metadata (`drive_nodes`/`drive_shares`/`drive_tokens`) lives in the
//! guardian-db relational mirror (`relational.rs`) — replicated to every
//! node, so `list`/`mkdir`/`move`/`delete`/`share` all read/write their own
//! local replica with no proxying: mutations reach here only after
//! `admin_ingress` has already forwarded them to the control-plane leader
//! (the same single-writer discipline `project_teams`/`billing_accounts`
//! already rely on — see relational.rs's module doc), and reads converge
//! within seconds via guardian-db's own CRDT replication.
//!
//! File BYTES are the one thing that is NOT replicated (`drive_blobs.rs`,
//! node-local content-addressed store) — `owner_node` on the `drive_nodes`
//! row names the node that received the `PUT` (in practice: whichever node
//! `admin_ingress` forwarded it to, since PUT is a mutation). A `GET` landing
//! on a different node (round-robin ingress) proxies to `owner_node` over the
//! SAME owner-routed, proxy-on-miss shape `browser_artifacts.rs` uses for its
//! content-addressed bytes: HTTP admin first, the iroh gossip transport as
//! the NAT'd-peer fallback (see `gossip.rs`'s `/v1/drive/mesh-file/` and
//! `/v1/drive/mesh-share/` arms).

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::admin::{fetch_bytes_from_host, mesh_team_qs, norm, require_project};
use crate::relational::{self, DriveNode, DriveWriteError};
use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route("/v1/drive/:project/list", get(list))
        .route("/v1/drive/:project/mkdir", post(mkdir))
        .route("/v1/drive/:project/file", get(get_file).put(put_file).delete(delete_file))
        .route("/v1/drive/:project/move", post(move_node))
        .route("/v1/drive/:project/share", post(share_create))
        .route("/v1/drive/:project/webdav-token", post(webdav_token_mint))
        .route("/v1/drive/share/:token", get(share_get))
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

/// Tenant's tier — the same team-row-else-billing-account fallback every
/// other quota-checking API module (`sandboxes_api::plan_of`) already
/// duplicates locally rather than sharing a private helper across modules.
pub(crate) fn plan_of(c: &Arc<CloudState>, team: &str) -> String {
    c.teams
        .get(team)
        .map(|t| t.plan)
        .unwrap_or_else(|| c.billing.account(team).plan)
}

fn split_path(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Walk a dir-only chain (`""` = root, always valid) to the id of the
/// directory at `path`. `Err` names which segment failed and why.
pub(crate) async fn resolve_dir_id(project: &str, path: &str) -> Result<String, (StatusCode, Json<Value>)> {
    let mut parent_id = String::new();
    for seg in split_path(path) {
        let node = relational::drive_resolve(project, &parent_id, &seg)
            .await
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no such directory: {seg}")))?;
        if node.kind != "dir" {
            return Err(err(StatusCode::BAD_REQUEST, format!("{seg} is not a directory")));
        }
        parent_id = node.id;
    }
    Ok(parent_id)
}

/// Resolve a full path (file or dir) to its node.
pub(crate) async fn resolve_full(project: &str, path: &str) -> Option<DriveNode> {
    let segs = split_path(path);
    let mut parent_id = String::new();
    let mut node: Option<DriveNode> = None;
    for (i, seg) in segs.iter().enumerate() {
        let n = relational::drive_resolve(project, &parent_id, seg).await?;
        if i + 1 < segs.len() {
            if n.kind != "dir" {
                return None;
            }
            parent_id = n.id.clone();
        }
        node = Some(n);
    }
    node
}

/// Split `path` into (existing parent dir id, last segment) — the shape both
/// `mkdir` (create the last segment) and `PUT file` (write the last segment)
/// need. The parent chain must already exist; this never creates
/// intermediate directories.
pub(crate) async fn resolve_parent(project: &str, path: &str) -> Result<(String, String), (StatusCode, Json<Value>)> {
    let mut segs = split_path(path);
    let name = segs
        .pop()
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "path is required"))?;
    let parent_path = segs.join("/");
    let parent_id = resolve_dir_id(project, &parent_path).await?;
    Ok((parent_id, name))
}

fn node_json(n: &DriveNode) -> Value {
    json!({
        "id": n.id,
        "name": n.name,
        "kind": n.kind,
        "size_bytes": n.size_bytes,
        "mime": n.mime,
        "content_hash": n.content_hash,
        "mtime": n.mtime,
        "ctime": n.ctime,
    })
}

#[derive(Deserialize)]
struct PathQ {
    #[serde(default)]
    path: String,
}

async fn list(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Query(q): Query<PathQ>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)
        .map_err(|(s, m)| err(s, m))?;
    let parent_id = resolve_dir_id(&project, &q.path).await?;
    let children = relational::drive_list_children(&project, &parent_id).await;
    Ok(Json(json!({
        "path": q.path,
        "entries": children.iter().map(node_json).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct MkdirReq {
    path: String,
}

async fn mkdir(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Json(body): Json<MkdirReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)
        .map_err(|(s, m)| err(s, m))?;
    let (parent_id, name) = resolve_parent(&project, &body.path).await?;
    match relational::drive_mkdir(&project, &parent_id, &name, &t).await {
        Ok(node) => Ok(Json(node_json(&node))),
        Err(DriveWriteError::WrongKind) => Err(err(
            StatusCode::CONFLICT,
            format!("{name} already exists and is not a directory"),
        )),
    }
}

/// Enforced BEFORE any bytes are written to the local blob store — a rejected
/// upload must never consume disk (design §4).
async fn check_quota(
    c: &Arc<CloudState>,
    project: &str,
    team: &str,
    incoming_bytes: u64,
) -> Result<(), (StatusCode, Json<Value>)> {
    let max = crate::billing::plan_max_drive_bytes(&plan_of(c, team));
    if max == 0 {
        return Ok(());
    }
    let used = relational::drive_bytes_used(project).await;
    if used.saturating_add(incoming_bytes) > max {
        return Err(err(
            StatusCode::PAYMENT_REQUIRED,
            format!(
                "drive quota exceeded: {used} + {incoming_bytes} bytes would exceed the {max}-byte plan limit — upgrade to add more"
            ),
        ));
    }
    Ok(())
}

async fn put_file(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Query(q): Query<PathQ>,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)
        .map_err(|(s, m)| err(s, m))?;
    let (parent_id, name) = resolve_parent(&project, &q.path).await?;
    check_quota(&c, &project, &t, body.len() as u64).await?;
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let content_hash = crate::drive_blobs::add_bytes(body.clone())
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("blob store: {e}")))?;
    let node = relational::drive_put_file(
        &project,
        &parent_id,
        &name,
        body.len() as u64,
        &mime,
        &content_hash,
        &t,
        &c.node_name,
    )
    .await
    .map_err(|_| err(StatusCode::CONFLICT, format!("{name} already exists and is not a file")))?;
    Ok(Json(node_json(&node)))
}

/// Exact-size + BLAKE3-hex check every served body must pass, local or
/// proxied — the `browser_artifacts::verify_bytes` discipline applied to
/// drive bytes.
fn verify_bytes(node: &DriveNode, bytes: &[u8]) -> bool {
    bytes.len() as u64 == node.size_bytes as u64 && crate::drive_blobs::matches(bytes, &node.content_hash)
}

fn file_response(node: &DriveNode, bytes: Vec<u8>) -> Response {
    let mut resp = (StatusCode::OK, bytes).into_response();
    let h = resp.headers_mut();
    let mime = if node.mime.is_empty() {
        "application/octet-stream"
    } else {
        &node.mime
    };
    if let Ok(v) = header::HeaderValue::from_str(mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = header::HeaderValue::from_str(&format!("\"{}\"", node.content_hash)) {
        h.insert(header::ETAG, v);
    }
    resp
}

/// Nodes that may hold the bytes: the recorded owner, then (best-effort) the
/// current control-plane leader — the `browser_artifacts::owner_candidates`
/// precedent, in case `owner_node` itself is unreachable but the leader
/// re-received a retried PUT and holds a newer copy.
fn owner_candidates(cloud: &Arc<CloudState>, owner_node: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !owner_node.is_empty() && owner_node != cloud.node_name {
        out.push(owner_node.to_string());
    }
    if !cloud.is_control_plane_leader() {
        let leader = cloud.control_plane_leader();
        if leader != cloud.node_name && !out.contains(&leader) {
            out.push(leader);
        }
    }
    out
}

async fn fetch_drive_file_bytes(
    cloud: &Arc<CloudState>,
    node_addr: &str,
    project: &str,
    path: &str,
    team: &str,
) -> Option<Vec<u8>> {
    let encoded = percent_encoding::utf8_percent_encode(path, percent_encoding::NON_ALPHANUMERIC);
    let rest_path = format!("/v1/drive/{project}/file?path={encoded}&local=true");
    if let Some(bytes) = fetch_bytes_from_host(cloud, node_addr, &rest_path, team).await {
        return Some(bytes);
    }
    let target = cloud
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == node_addr)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
    if let Some((id, addr)) = target {
        let p = format!(
            "/v1/drive/mesh-file/?project={project}&path={encoded}&{}",
            mesh_team_qs(team)
        );
        return crate::gossip::request_to(cloud, &id, &addr, hive_p2p::GOSSIP_GET, &p, &[], 20).await;
    }
    None
}

#[derive(Deserialize)]
struct LocalPathQ {
    #[serde(default)]
    path: String,
    #[serde(default)]
    local: Option<bool>,
}

/// Resolve `path` to its (live) file node and its verified bytes — local
/// disk first, then every owner candidate over the proxy transport. Shared by
/// the REST `GET .../file` handler and `drive_webdav.rs`'s read-mode `open`,
/// so both surfaces get the exact same owner-routed, proxy-on-miss behavior.
pub(crate) async fn resolve_and_fetch_bytes(
    c: &Arc<CloudState>,
    project: &str,
    path: &str,
    tenant: &str,
) -> Option<(DriveNode, Vec<u8>)> {
    let node = resolve_full(project, path).await.filter(|n| n.kind == "file")?;
    if let Ok(bytes) = crate::drive_blobs::read_local(&node.content_hash, node.size_bytes as u64).await {
        if verify_bytes(&node, &bytes) {
            return Some((node, bytes));
        }
    }
    for candidate in owner_candidates(c, &node.owner_node) {
        let Some(bytes) = fetch_drive_file_bytes(c, &candidate, project, path, tenant).await else {
            continue;
        };
        if !bytes.is_empty() && verify_bytes(&node, &bytes) {
            return Some((node, bytes));
        }
    }
    None
}

async fn get_file(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Query(q): Query<LocalPathQ>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let t = require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)
        .map_err(|(s, m)| err(s, m))?;
    if q.local == Some(true) {
        let node = resolve_full(&project, &q.path)
            .await
            .filter(|n| n.kind == "file")
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "file not found"))?;
        let bytes = crate::drive_blobs::read_local(&node.content_hash, node.size_bytes as u64)
            .await
            .ok()
            .filter(|b| verify_bytes(&node, b))
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "file bytes are not persisted on this node"))?;
        return Ok(file_response(&node, bytes));
    }
    let (node, bytes) = resolve_and_fetch_bytes(&c, &project, &q.path, &t)
        .await
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "file not found or its bytes are unavailable on every owning node"))?;
    Ok(file_response(&node, bytes))
}

/// Serving half of the `/v1/drive/mesh-file/` gossip arm.
pub async fn mesh_get_file(cloud: &Arc<CloudState>, tenant: &str, project: &str, path: &str) -> Vec<u8> {
    let tenant = norm(tenant).to_string();
    if !crate::admin::project_owned_by(cloud, project, &tenant) {
        return Vec::new();
    }
    let Some(node) = resolve_full(project, path).await.filter(|n| n.kind == "file") else {
        return Vec::new();
    };
    crate::drive_blobs::read_local(&node.content_hash, node.size_bytes as u64)
        .await
        .ok()
        .filter(|b| verify_bytes(&node, b))
        .unwrap_or_default()
}

async fn delete_file(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Query(q): Query<PathQ>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)
        .map_err(|(s, m)| err(s, m))?;
    let node = resolve_full(&project, &q.path)
        .await
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "not found"))?;
    if node.kind == "dir" && !relational::drive_list_children(&project, &node.id).await.is_empty() {
        return Err(err(StatusCode::CONFLICT, "directory is not empty"));
    }
    relational::drive_tombstone(&node.id).await;
    Ok(Json(json!({ "deleted": node.id })))
}

#[derive(Deserialize)]
struct MoveReq {
    from: String,
    to: String,
}

async fn move_node(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Json(body): Json<MoveReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)
        .map_err(|(s, m)| err(s, m))?;
    let node = resolve_full(&project, &body.from)
        .await
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "source not found"))?;
    let (new_parent_id, new_name) = resolve_parent(&project, &body.to).await?;
    if relational::drive_resolve(&project, &new_parent_id, &new_name).await.is_some() {
        return Err(err(StatusCode::CONFLICT, "destination already exists"));
    }
    relational::drive_move(&node.id, &new_parent_id, &new_name).await;
    Ok(Json(json!({ "moved": node.id, "to": body.to })))
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn rand_token(prefix: &str) -> String {
    format!(
        "{prefix}_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[derive(Deserialize)]
struct ShareReq {
    path: String,
    #[serde(default = "default_scope")]
    scope: String,
    /// Seconds from now until the link expires.
    ttl: u64,
}
fn default_scope() -> String {
    "read".into()
}

async fn share_create(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Json(body): Json<ShareReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)
        .map_err(|(s, m)| err(s, m))?;
    if body.scope != "read" && body.scope != "write" {
        return Err(err(StatusCode::BAD_REQUEST, "scope must be read or write"));
    }
    let node = resolve_full(&project, &body.path)
        .await
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "not found"))?;
    let token = rand_token("dshr");
    let expires_at = hive_core::now_ms() as i64 + (body.ttl.min(365 * 24 * 3600) as i64 * 1000);
    let id = relational::drive_share_mint(&node.id, &sha256_hex(&token), &body.scope, expires_at).await;
    Ok(Json(json!({
        "id": id,
        "token": token,
        "share_url": format!("/v1/drive/share/{token}"),
        "scope": body.scope,
        "expires_at": expires_at,
    })))
}

#[derive(Deserialize)]
struct ShareLocalQ {
    #[serde(default)]
    local: Option<bool>,
}

/// Resolve+serve a share token — file bytes only (design §2: "unauthenticated
/// read via the share token only"). A `write`-scope grant is recorded (the
/// data model is share-scope-complete for a future write-consuming endpoint)
/// but this route always only reads; there is no anonymous write path in v1.
async fn share_get(
    State(c): State<Arc<CloudState>>,
    Path(token): Path<String>,
    Query(q): Query<ShareLocalQ>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let (node, _grant) = resolve_share(&token).await.ok_or_else(|| err(StatusCode::NOT_FOUND, "invalid or expired share link"))?;
    if let Ok(bytes) = crate::drive_blobs::read_local(&node.content_hash, node.size_bytes as u64).await {
        if verify_bytes(&node, &bytes) {
            return Ok(file_response(&node, bytes));
        }
    }
    if q.local == Some(true) {
        return Err(err(StatusCode::NOT_FOUND, "file bytes are not persisted on this node"));
    }
    for candidate in owner_candidates(&c, &node.owner_node) {
        let Some(bytes) = fetch_drive_share_bytes(&c, &candidate, &token).await else {
            continue;
        };
        if !bytes.is_empty() && verify_bytes(&node, &bytes) {
            return Ok(file_response(&node, bytes));
        }
    }
    Err(err(StatusCode::NOT_FOUND, "file bytes are unavailable on every owning node"))
}

/// `token` -> (live target node, its grant), independently re-derivable by
/// every node from the SAME replicated `drive_shares`/`drive_nodes` rows —
/// what makes anonymous cross-node proxying safe with no caller-trusted state.
async fn resolve_share(token: &str) -> Option<(DriveNode, relational::DriveShare)> {
    let grant = relational::drive_share_resolve(&sha256_hex(token)).await?;
    if grant.expires_at < hive_core::now_ms() as i64 {
        return None;
    }
    let node = relational::drive_get(&grant.node_id).await?;
    if node.deleted_at != 0 || node.kind != "file" {
        return None;
    }
    Some((node, grant))
}

async fn fetch_drive_share_bytes(cloud: &Arc<CloudState>, node_addr: &str, token: &str) -> Option<Vec<u8>> {
    let rest_path = format!("/v1/drive/share/{token}?local=true");
    if let Some(bytes) = fetch_bytes_from_host(cloud, node_addr, &rest_path, "").await {
        return Some(bytes);
    }
    let target = cloud
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == node_addr)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
    if let Some((id, addr)) = target {
        let p = format!("/v1/drive/mesh-share/?token={token}");
        return crate::gossip::request_to(cloud, &id, &addr, hive_p2p::GOSSIP_GET, &p, &[], 20).await;
    }
    None
}

/// Serving half of the `/v1/drive/mesh-share/` gossip arm.
pub async fn mesh_share_get(token: &str) -> Vec<u8> {
    let Some((node, _)) = resolve_share(token).await else {
        return Vec::new();
    };
    crate::drive_blobs::read_local(&node.content_hash, node.size_bytes as u64)
        .await
        .ok()
        .filter(|b| verify_bytes(&node, b))
        .unwrap_or_default()
}

/// Mint (or rotate) this project's one WebDAV Basic-auth credential — shown
/// once, stored only as a SHA-256 hash (`apikeys.rs`'s own convention),
/// checked by `drive_webdav.rs` on every mount request.
async fn webdav_token_mint(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_project(&c, &headers, claims.as_ref().map(|e| &e.0), &project)
        .map_err(|(s, m)| err(s, m))?;
    let token = rand_token("dat");
    relational::drive_token_set(&project, &sha256_hex(&token)).await;
    Ok(Json(json!({
        "project": project,
        "username": project,
        "password": token,
        "webdav_url": format!("/v1/drive/webdav/{project}"),
    })))
}

/// Verify a presented WebDAV Basic-auth (username, password) pair against
/// `project`'s stored token — `drive_webdav.rs`'s own auth gate.
pub(crate) async fn verify_webdav_credentials(project: &str, username: &str, password: &str) -> bool {
    if username != project {
        return false;
    }
    relational::drive_token_verify(project, &sha256_hex(password)).await
}
