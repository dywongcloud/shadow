//! Sandboxes HTTP API: project-scoped CRUD for sandboxes, commands, snapshots, mounts and network
//! policy (`require_project` tenant/project authz, audit + persist after every write).
//!
//! # Placement and owner routing
//!
//! Every sandbox MUTATION is forwarded by `admin_ingress` to the control-plane leader, which may
//! have NO exec-capable isolation backend (fc-sanjose runs Mock), so the leader is a ROUTER, never
//! a fallback provisioner:
//!
//! - `create_sandbox` provisions locally only when this node passes `sandbox_exec_capable`;
//! otherwise it mints the id and forwards the create to candidate owners (healthy, not self,
//! backend in {firecracker, litebox}; same region first, then by name) one at a time over
//! `internal_hop` (HTTP admin if known, else the iroh gossip `POST` arm), each at most once under
//! per-peer and total budgets. The leader-minted id makes a retried create idempotent on the owner;
//! with no taker the caller gets a typed 503 `SANDBOX_NO_CAPABLE_NODE` naming every candidate and
//! why. The winning record (`owner_node` = that peer) is ADOPTED on the leader as metadata, so
//! later mutations can route.
//! - `start`/`restart`/`stop`/`delete`/`run`/`kill` re-route to `SandboxRecord::owner_node` through
//! ONE typed RPC (`owner_op`, always `POST`); the owner re-authorizes against its own record and
//! never re-proxies. Reads reach the owner through `proxy_read`. The shell (a websocket) cannot be
//! proxied: a non-owner tunnels it to the owner over the `RawTarget` mesh surface (`mesh_shell`)
//! and answers `wrong_node` only when no mesh path exists.

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::admin::{require_project, tenant};
use crate::sandboxes::*;
use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;

/// Internal owner-hop paths, fleet service credentials only (`require_internal_hop`); exempt from
/// leader forwarding (`main.rs`'s `owner_routed`) and served over the mesh by matching
/// `gossip::dispatch` arms.
pub(crate) const DELEGATE_CREATE_PATH: &str = "/v1/internal/sandboxes/delegate-create";
pub(crate) const OWNER_OP_PATH: &str = "/v1/internal/sandboxes/owner-op";

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route(
            "/v1/projects/:project/sandboxes",
            get(list_sandboxes).post(create_sandbox),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id",
            get(get_sandbox).patch(patch_sandbox).delete(delete_sandbox),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/stop",
            post(stop_sandbox),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/start",
            post(start_sandbox),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/restart",
            post(restart_sandbox),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/commands",
            post(run_command).get(list_commands),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/commands/:command_id",
            get(get_command),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/commands/:command_id/logs",
            get(get_command_logs),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/commands/:command_id/kill",
            post(kill_command),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/files/write",
            post(write_files),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/files/read",
            get(read_file),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/snapshots",
            post(create_snapshot).get(list_snapshots),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/snapshots/:snapshot_id",
            delete(delete_snapshot),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/mounts",
            post(create_mount).get(list_mounts),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/mounts/:mount_id",
            delete(delete_mount),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/network-policy",
            put(update_network_policy),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/domain",
            get(get_domain),
        )
        .route(
            "/v1/projects/:project/sandboxes/:sandbox_id/shell",
            get(open_shell),
        )
        .route(DELEGATE_CREATE_PATH, post(delegate_create_sandbox))
        .route(OWNER_OP_PATH, post(owner_op))
}

fn sandbox_err(e: SandboxError) -> (StatusCode, String) {
    let status = match &e {
        SandboxError::NotFound(_)
        | SandboxError::CommandNotFound(_)
        | SandboxError::SnapshotNotFound(_)
        | SandboxError::MountNotFound(_) => StatusCode::NOT_FOUND,
        SandboxError::Unauthorized(_) => StatusCode::FORBIDDEN,
        SandboxError::AlreadyExists(_) => StatusCode::CONFLICT,
        SandboxError::QuotaExceeded(_) => StatusCode::PAYMENT_REQUIRED,
        SandboxError::EngineUnavailable(_)
        | SandboxError::NoCapableNode(_)
        | SandboxError::OwnerUnreachable(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        json!({ "code": e.code(), "error": e.message() }).to_string(),
    )
}

fn bad(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, json!({ "error": msg }).to_string())
}

fn plan_of(c: &Arc<CloudState>, team: &str) -> String {
    c.teams
        .get(team)
        .map(|t| t.plan)
        .unwrap_or_else(|| c.billing.account(team).plan)
}

/// Authorize + resolve tenant for a project-scoped sandbox route in one call.
///
/// A node that never saw `project` (ProjectSettings rows replicate eventually) gets
/// `UNTAGGED_TENANT` from `team_of`, which can never equal a real tenant, so rejecting outright
/// would 403 a legitimate cross-node read before it reaches the `proxy_read` fallback. The caller's
/// own tenant claim is trusted PROVISIONALLY then: mutations only run this body on the leader
/// (`admin_ingress` forwards every POST/PUT/DELETE/PATCH there), whose project row is accurate for
/// anything it mutated, and a read falls through to `proxy_read`, whose target re-runs this check
/// against ITS row and rejects a wrong-team caller there.
fn require(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    claims: &Claims,
    project: &str,
) -> Result<String, (StatusCode, String)> {
    if c.projects.team_of(project) == crate::admin::UNTAGGED_TENANT {
        return Ok(tenant(c, headers, claims.as_ref().map(|e| &e.0)));
    }
    require_project(c, headers, claims.as_ref().map(|e| &e.0), project)
}

/// Gate for the internal owner hops: the fleet `x-hive-internal` token, OR a `mesh-internal`
/// service JWT whose tenant claim matches the tenant the body asserts (a hop can never act for a
/// tenant its credential was not minted for). With NEITHER credential configured the hop is open,
/// like `auth::require_auth`'s dev mode.
fn require_internal_hop(
    headers: &HeaderMap,
    claims: &Claims,
    tenant: &str,
) -> Result<(), (StatusCode, String)> {
    let internal_token = std::env::var("HIVE_INTERNAL_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    if let Some(expected) = internal_token.as_deref() {
        if headers
            .get("x-hive-internal")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|got| crate::admin::ct_eq(got, expected))
        {
            return Ok(());
        }
    }
    if let Some(cl) = claims.as_ref().map(|e| &e.0) {
        if crate::auth::is_mesh_internal(cl)
            && crate::admin::norm(&cl.tenant) == crate::admin::norm(tenant)
        {
            return Ok(());
        }
    }
    if !crate::auth::enforced() && internal_token.is_none() {
        return Ok(());
    }
    Err((
        StatusCode::FORBIDDEN,
        json!({ "error": "internal sandbox hop requires fleet service credentials" }).to_string(),
    ))
}

/// A sandbox view without secrets: masks `env_refs` values (already ciphertext) like every other
/// endpoint that never echoes secret material.
fn masked_sandbox(mut s: SandboxRecord) -> SandboxRecord {
    for e in &mut s.env_refs {
        e.value_enc = "••••••••".to_string();
    }
    s
}

/// The node that holds `sandbox_id`'s live cell, if this node's own record
/// says it is SOMEONE ELSE. `None` = no local record, or the record is owned
/// here, or it predates `owner_node` (empty → the leader-placement rule).
async fn remote_owner(c: &Arc<CloudState>, project: &str, sandbox_id: &str) -> Option<String> {
    let rec = c.sandboxes.get_sandbox(project, sandbox_id).await.ok()?;
    if rec.owner_node.is_empty() || rec.owner_node == c.node_name {
        None
    } else {
        Some(rec.owner_node)
    }
}

/// The node that must execute a cell-touching MUTATION for `sandbox_id`, or `None` when that is
/// this node. `remote_owner` answers only from THIS node's record, and a node that never adopted
/// the sandbox has none (`start` 404'd where the equivalent GET answered through `read_target`), so
/// this asks the leader, which holds an adopted copy of every record.
async fn mutation_owner(
    c: &Arc<CloudState>,
    project: &str,
    sandbox_id: &str,
    team: &str,
) -> Option<String> {
    if let Some(owner) = remote_owner(c, project, sandbox_id).await {
        return Some(owner);
    }
    if c.sandboxes.get_sandbox(project, sandbox_id).await.is_ok() {
        return None;
    }
    let leader = c.leader_forward_target()?;
    if leader == c.node_name {
        return None;
    }
    let v = proxy_read(
        c,
        &leader,
        &format!("/v1/projects/{project}/sandboxes/{sandbox_id}"),
        team,
    )
    .await?;
    let owner = v.get("owner_node")?.as_str()?.to_string();
    if owner.is_empty() || owner == c.node_name {
        None
    } else {
        Some(owner)
    }
}

/// Run a cell-touching mutation on the node that owns the sandbox. `Ok(None)`
/// means "no remote owner — apply it locally"; `Ok(Some(rec))` is the owner's
/// record, adopted here so every later hop and read finds it.
async fn forward_mutation(
    c: &Arc<CloudState>,
    project: &str,
    sandbox_id: &str,
    team: &str,
    op: &str,
) -> Result<Option<SandboxRecord>, (StatusCode, String)> {
    let Some(owner) = mutation_owner(c, project, sandbox_id, team).await else {
        return Ok(None);
    };
    let req = OwnerOpReq {
        tenant: team.to_string(),
        project: project.to_string(),
        sandbox_id: sandbox_id.to_string(),
        op: op.to_string(),
        command_id: String::new(),
        run: None,
    };
    let v = forward_owner_op(c, &owner, team, &req, owner_op_budget()).await?;
    let rec: SandboxRecord = owner_reply_field(&v, "sandbox", &owner)?;
    c.sandboxes.adopt_record(rec.clone());
    c.audit.record(
        team,
        "user",
        op,
        "sandbox",
        sandbox_id,
        &format!("owner: {owner}"),
    );
    crate::persist::persist(c);
    Ok(Some(rec))
}

/// Where a READ for `sandbox_id` should go when this node cannot answer it:
/// the record's remote owner if known here, else the control-plane leader
/// (which holds an adopted copy of every record and can hop again), else
/// nowhere (this node IS the leader and knows nothing).
async fn read_target(c: &Arc<CloudState>, project: &str, sandbox_id: &str) -> Option<String> {
    if let Some(owner) = remote_owner(c, project, sandbox_id).await {
        return Some(owner);
    }
    c.leader_forward_target()
}

/// Cross-node fallback for sandbox READS (the `fetch_from_host` precedent):
/// GET `path` from `node` with the caller's tenant authority. `None` on any
/// transport failure so callers fall back to their own not-found.
async fn proxy_read(c: &Arc<CloudState>, node: &str, path: &str, team: &str) -> Option<Value> {
    if node == c.node_name {
        return None;
    }
    crate::admin::fetch_from_host(c, node, path, team).await
}

/// True for every "not found" flavor of `SandboxError`: on a NON-owner node a missing
/// command/snapshot/mount as likely means "this node never had the sandbox", which the proxy
/// fallback resolves (a real 404 on the owner still 404s).
fn is_not_found(e: &SandboxError) -> bool {
    matches!(
        e,
        SandboxError::NotFound(_)
            | SandboxError::CommandNotFound(_)
            | SandboxError::SnapshotNotFound(_)
            | SandboxError::MountNotFound(_)
    )
}

/// Outcome of one internal owner hop. The three arms are DISTINCT on purpose:
/// a typed refusal from the peer's handler (it did not apply the request) must
/// not be confused with a dead transport (unknown whether it applied).
enum Hop {
    /// The peer applied the request and answered with this JSON.
    Ok(Value),
    /// The peer's handler refused with this exact status + body.
    Refused(StatusCode, String),
    /// Transport failure, timeout, or a pre-upgrade peer answering NO_HANDLER.
    Unreachable(String),
}

fn env_ms(key: &str, default_ms: u64) -> Duration {
    Duration::from_millis(
        std::env::var(key)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default_ms),
    )
}

/// Per-candidate budget for a delegated create: covers one HTTP-admin attempt
/// (15s) plus the mesh fallback (20s dial budget) with headroom for a real
/// Firecracker boot. `HIVE_SANDBOX_DELEGATE_PEER_MS`.
fn delegate_peer_budget() -> Duration {
    env_ms("HIVE_SANDBOX_DELEGATE_PEER_MS", 40_000)
}

/// Whole-create budget across every candidate — bounds the worst case (many
/// capable-but-wedged peers) below the admin request timeout (120s) so the
/// caller gets the typed refusal, not a proxy timeout.
/// `HIVE_SANDBOX_DELEGATE_TOTAL_MS`.
fn delegate_total_budget() -> Duration {
    env_ms("HIVE_SANDBOX_DELEGATE_TOTAL_MS", 90_000)
}

/// Budget for a stop/delete/kill/run hop to a known owner.
/// `HIVE_SANDBOX_OWNER_OP_MS`. A blocking `run` (non-detached) can legitimately
/// take as long as the command, so the caller passes its own bound.
fn owner_op_budget() -> Duration {
    env_ms("HIVE_SANDBOX_OWNER_OP_MS", 45_000)
}

/// One bounded POST to `node`'s internal sandbox surface with the caller's tenant authority: the
/// HTTP admin URL when known, else the iroh gossip `POST` arm; same credential shape as
/// `admin::post_to_host_json`. The peer's status and body are preserved, so a typed refusal is
/// distinguishable from a dead transport.
async fn internal_hop(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
    body: &Value,
    budget: Duration,
) -> Hop {
    match tokio::time::timeout(budget, internal_hop_inner(c, node, path, team, body)).await {
        Ok(hop) => hop,
        Err(_) => Hop::Unreachable(format!("no answer within {}ms", budget.as_millis())),
    }
}

async fn internal_hop_inner(
    c: &Arc<CloudState>,
    node: &str,
    path: &str,
    team: &str,
    body: &Value,
) -> Hop {
    let admin = c.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        let mut request = c
            .http
            .post(format!("{admin}{path}"))
            .header("x-hive-team", team);
        if let Ok(token) = std::env::var("HIVE_INTERNAL_TOKEN") {
            if !token.trim().is_empty() {
                request = request.header("x-hive-internal", token);
            }
        }
        if crate::auth::enforced() {
            if let Ok(token) = crate::auth::issue(
                "mesh-internal",
                team,
                "service",
                false,
                crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
            ) {
                request = request.bearer_auth(token);
            }
        }
        if let Ok(r) = request
            .timeout(Duration::from_secs(15))
            .json(body)
            .send()
            .await
        {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            if status.is_success() {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    return Hop::Ok(v);
                }
                return Hop::Unreachable(format!("{node} answered 2xx with a non-JSON body"));
            }
            return Hop::Refused(status, text);
        }
    }
    let target = c
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == node)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
    let Some((id, addr)) = target else {
        return Hop::Unreachable(format!(
            "{node} has no reachable HTTP admin URL and no iroh address in the registry"
        ));
    };
    let sep = if path.contains('?') { '&' } else { '?' };
    let p = format!("{path}{sep}{}", crate::admin::mesh_team_qs(team));
    let body_bytes = serde_json::to_vec(body).unwrap_or_default();
    match crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &p, &body_bytes, 20).await
    {
        None => Hop::Unreachable(format!("mesh request to {node} failed or timed out")),
        Some(b) if b.is_empty() => Hop::Unreachable(format!(
            "{node} answered NO_HANDLER (a pre-upgrade binary without the sandbox owner RPC)"
        )),
        Some(b) => match serde_json::from_slice::<Value>(&b) {
            Ok(v) => match v.get("__refused") {
                Some(r) => {
                    let status = r
                        .get("status")
                        .and_then(|s| s.as_u64())
                        .and_then(|s| StatusCode::from_u16(s as u16).ok())
                        .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
                    let body = r
                        .get("body")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    Hop::Refused(status, body)
                }
                None => Hop::Ok(v),
            },
            Err(e) => Hop::Unreachable(format!("{node} answered an unparseable mesh reply: {e}")),
        },
    }
}

/// The mesh-side envelope for a handler refusal (`gossip::dispatch` wraps an
/// `Err((status, body))` in this so `internal_hop` can reconstruct it).
pub(crate) fn refused_envelope(status: StatusCode, body: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "__refused": { "status": status.as_u16(), "body": body } }))
        .unwrap_or_default()
}

/// Candidate owners for a create this node cannot provision: healthy registry nodes other than this
/// one whose gossiped `backend` passes `sandbox_exec_capable` (the predicate of
/// `PlatformSandboxProvider::local_exec_capable`), ordered same region first, then by name, so
/// retries and every observer agree.
fn capable_peers(c: &Arc<CloudState>) -> Vec<String> {
    let mut candidates: Vec<(bool, String)> = c
        .registry
        .nodes()
        .into_iter()
        .filter(|n| {
            !n.is_self && n.name != c.node_name && n.healthy && sandbox_exec_capable(&n.backend)
        })
        .map(|n| (n.region != c.region, n.name))
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates.into_iter().map(|(_, name)| name).collect()
}

fn summarize_refusal(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("");
            let err = v.get("error").and_then(|e| e.as_str())?;
            Some(if code.is_empty() {
                err.to_string()
            } else {
                format!("{code}: {err}")
            })
        })
        .unwrap_or_else(|| body.chars().take(200).collect())
}

/// Lists from this node's records; when there are none it asks the leader, which adopts a copy of
/// every record and is the one node whose list is complete.
pub(crate) async fn list_sandboxes(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let local = c
        .sandboxes
        .list_sandboxes(&project)
        .await
        .map_err(sandbox_err)?;
    if let (true, Some(leader)) = (local.is_empty(), c.leader_forward_target()) {
        if let Some(v) = proxy_read(
            &c,
            &leader,
            &format!("/v1/projects/{project}/sandboxes"),
            &t,
        )
        .await
        {
            return Ok(Json(v));
        }
    }
    Ok(Json(
        json!({ "sandboxes": local.into_iter().map(masked_sandbox).collect::<Vec<_>>() }),
    ))
}

#[derive(Deserialize)]
struct CreateReq {
    name: String,
    #[serde(default = "default_runtime")]
    runtime: String,
    #[serde(default = "default_vcpus")]
    vcpus: u32,
    #[serde(default)]
    memory_mb: u32,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    persistent: bool,
    #[serde(default)]
    ports: Vec<u16>,
    #[serde(default)]
    network_policy: NetworkPolicy,
    #[serde(default)]
    env: Vec<EnvVarReq>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    source_kind: String,
    #[serde(default)]
    source_ref: String,
}
fn default_runtime() -> String {
    "node22".into()
}
fn default_vcpus() -> u32 {
    1
}
fn default_timeout_ms() -> u64 {
    5 * 60_000
}

#[derive(Deserialize)]
struct EnvVarReq {
    key: String,
    value: String,
    #[serde(default)]
    sensitive: bool,
}

/// Wire body of a leader→owner delegated create. `id` is minted by the
/// leader BEFORE transport (idempotency key on the owner); `input` is exactly
/// the validated `CreateSandboxInput` the leader would have provisioned from.
#[derive(Deserialize, Serialize)]
pub(crate) struct DelegateCreateReq {
    tenant: String,
    project: String,
    id: String,
    input: CreateSandboxInput,
}

/// Owner side of a delegated create: provision THIS exact sandbox here (this node has an
/// exec-capable backend, the leader does not). Fail-closed (a backend that cannot exec answers
/// `EngineUnavailable`, so the leader moves on), idempotent on `id`, `owner_node` stamped with this
/// node. Returns the UNMASKED record (env values are already sealed): the leader adopts it as-is
/// and the public handler masks.
pub(crate) async fn delegate_create_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<DelegateCreateReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_internal_hop(&headers, &claims, &b.tenant)?;
    if !c.sandboxes.local_exec_capable() {
        return Err(sandbox_err(SandboxError::EngineUnavailable(format!(
            "node {} runs backend={} and cannot host a sandbox cell",
            c.node_name,
            c.sandboxes.backend_name().unwrap_or("none")
        ))));
    }
    let rec = c
        .sandboxes
        .create_sandbox_with_id(&b.tenant, &b.project, &b.id, b.input)
        .await
        .map_err(sandbox_err)?;
    c.audit.record(
        &b.tenant,
        "user",
        "create",
        "sandbox",
        &rec.id,
        &format!("{} (delegated owner: {})", rec.name, c.node_name),
    );
    crate::persist::persist(&c);
    Ok(Json(json!(rec)))
}

/// Validates before any placement decision (a bad request never burns a delegation round trip),
/// then checks name uniqueness HERE: the leader holds an adopted copy of every record of the
/// project, so the check is authoritative. A peer refusal that blames the REQUEST (4xx: conflict,
/// quota, invalid input) is passed through, since no other candidate would accept it; any other
/// refusal or a dead transport moves to the next candidate.
async fn create_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path(project): Path<String>,
    Json(b): Json<CreateReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let plan = plan_of(&c, &t);

    if b.name.trim().is_empty() {
        return Err(bad("name is required"));
    }
    let max_sbx = crate::billing::plan_max_sandboxes(&plan);
    if max_sbx > 0 {
        let count = c.sandboxes.count_for_project(&project);
        if count >= max_sbx {
            return Err(sandbox_err(SandboxError::QuotaExceeded(format!("sandbox limit reached ({count}/{max_sbx}) on the {plan} plan — upgrade to add more"))));
        }
    }
    let max_running = crate::billing::plan_max_running_sandboxes(&plan);
    if max_running > 0 {
        let running = c.sandboxes.count_running_for_project(&project);
        if running >= max_running {
            return Err(sandbox_err(SandboxError::QuotaExceeded(format!("running-sandbox limit reached ({running}/{max_running}) on the {plan} plan — stop one or upgrade"))));
        }
    }
    let max_env = crate::billing::plan_max_sandbox_env_vars(&plan);
    if max_env > 0 && b.env.len() as u32 > max_env {
        return Err(sandbox_err(SandboxError::QuotaExceeded(format!(
            "{} env vars requested exceeds the {plan} plan limit of {max_env}",
            b.env.len()
        ))));
    }
    let max_ports = crate::billing::plan_max_sandbox_ports(&plan);
    if max_ports > 0 && b.ports.len() as u32 > max_ports {
        return Err(sandbox_err(SandboxError::QuotaExceeded(format!(
            "{} ports requested exceeds the {plan} plan limit of {max_ports}",
            b.ports.len()
        ))));
    }

    let input = CreateSandboxInput {
        name: b.name,
        runtime: b.runtime,
        vcpus: b.vcpus,
        memory_mb: b.memory_mb,
        timeout_ms: b.timeout_ms,
        persistent: b.persistent,
        ports: b.ports,
        network_policy: b.network_policy,
        env: b
            .env
            .into_iter()
            .map(|e| (e.key, e.value, e.sensitive))
            .collect(),
        tags: b.tags,
        source_kind: b.source_kind,
        source_ref: b.source_ref,
    };
    validate_name(&input.name).map_err(sandbox_err)?;
    validate_runtime(&input.runtime).map_err(sandbox_err)?;
    validate_network_policy(&input.network_policy).map_err(sandbox_err)?;
    if c.sandboxes
        .list_sandboxes(&project)
        .await
        .map_err(sandbox_err)?
        .iter()
        .any(|s| s.name == input.name)
    {
        return Err(sandbox_err(SandboxError::AlreadyExists(format!(
            "sandbox '{}' already exists in this project",
            input.name
        ))));
    }

    let id = crate::sandboxes_platform::mint_sandbox_id();

    if c.sandboxes.local_exec_capable() {
        let rec = c
            .sandboxes
            .create_sandbox_with_id(&t, &project, &id, input)
            .await
            .map_err(sandbox_err)?;
        c.audit
            .record(&t, "user", "create", "sandbox", &rec.id, &rec.name);
        crate::persist::persist(&c);
        return Ok(Json(json!(masked_sandbox(rec))));
    }

    let candidates = capable_peers(&c);
    if candidates.is_empty() {
        return Err(sandbox_err(SandboxError::NoCapableNode(format!(
            "no healthy node advertises an exec-capable sandbox backend (firecracker|litebox); this node ({}) runs backend={} — the create was applied nowhere",
            c.node_name,
            c.sandboxes.backend_name().unwrap_or("none")
        ))));
    }
    let body = json!(DelegateCreateReq {
        tenant: t.clone(),
        project: project.clone(),
        id: id.clone(),
        input,
    });
    let per_peer = delegate_peer_budget();
    let deadline = Instant::now() + delegate_total_budget();
    let mut tried: Vec<String> = Vec::with_capacity(candidates.len());
    for peer in &candidates {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tried.push(format!(
                "{peer}: not tried (total delegation budget exhausted)"
            ));
            continue;
        }
        let budget = per_peer.min(remaining);
        match internal_hop(&c, peer, DELEGATE_CREATE_PATH, &t, &body, budget).await {
            Hop::Ok(v) => match serde_json::from_value::<SandboxRecord>(v) {
                Ok(rec) if rec.id == id && rec.owner_node == *peer => {
                    c.sandboxes.adopt_record(rec.clone());
                    c.audit.record(
                        &t,
                        "user",
                        "create",
                        "sandbox",
                        &rec.id,
                        &format!("{} (owner: {peer})", rec.name),
                    );
                    crate::persist::persist(&c);
                    tracing::info!(sandbox = %rec.id, owner = %peer, "sandbox create delegated to a capable peer");
                    return Ok(Json(json!(masked_sandbox(rec))));
                }
                Ok(rec) => tried.push(format!(
                    "{peer}: answered with record {}/owner {} instead of {id}/{peer}",
                    rec.id, rec.owner_node
                )),
                Err(e) => tried.push(format!("{peer}: unparseable record: {e}")),
            },
            Hop::Refused(status, refusal) => {
                if status.is_client_error() {
                    return Err((status, refusal));
                }
                tried.push(format!(
                    "{peer}: refused {} ({})",
                    status.as_u16(),
                    summarize_refusal(&refusal)
                ));
            }
            Hop::Unreachable(why) => tried.push(format!("{peer}: {why}")),
        }
    }
    tracing::warn!(project = %project, sandbox = %id, tried = ?tried, "sandbox create applied on no capable node");
    Err(sandbox_err(SandboxError::NoCapableNode(format!(
        "sandbox create was applied on no node; candidates tried: {}",
        tried.join("; ")
    ))))
}

pub(crate) async fn get_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    match c.sandboxes.get_sandbox(&project, &sandbox_id).await {
        Ok(rec) => Ok(Json(json!(masked_sandbox(rec)))),
        Err(e) if is_not_found(&e) => {
            if let Some(node) = read_target(&c, &project, &sandbox_id).await {
                if let Some(v) = proxy_read(
                    &c,
                    &node,
                    &format!("/v1/projects/{project}/sandboxes/{sandbox_id}"),
                    &t,
                )
                .await
                {
                    return Ok(Json(v));
                }
            }
            Err(sandbox_err(e))
        }
        Err(e) => Err(sandbox_err(e)),
    }
}

#[derive(Deserialize)]
struct PatchReq {
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default, alias = "keepLastSnapshots")]
    keep_last_snapshots: Option<u32>,
}

/// Serves the record and REFUSES (400) a write it cannot apply, never answering 200 over a dropped
/// field: `tags`/`keep_last_snapshots` are the only cosmetic fields without a dedicated verb and
/// have no provider setter, so a caller must not find the old tags on the next read. Authz only
/// otherwise: nothing is written, so there is no audit row.
async fn patch_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Json(b): Json<PatchReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require(&c, &headers, &claims, &project)?;
    let rec = c
        .sandboxes
        .get_sandbox(&project, &sandbox_id)
        .await
        .map_err(sandbox_err)?;
    if b.tags.is_some() || b.keep_last_snapshots.is_some() {
        let mut fields: Vec<&str> = Vec::new();
        if b.tags.is_some() {
            fields.push("tags");
        }
        if b.keep_last_snapshots.is_some() {
            fields.push("keep_last_snapshots");
        }
        return Err(bad(&format!(
            "sandbox PATCH does not support {} yet — no provider setter exists (delete and recreate to change {})",
            fields.join(" / "),
            if fields.len() == 1 { "it" } else { "them" }
        )));
    }
    Ok(Json(json!(masked_sandbox(rec))))
}

/// The ONE typed internal owner RPC every cell-touching mutation rides (`op` names the operation),
/// always a `POST`. The owner re-derives authority from ITS OWN record (tenant must match,
/// `owner_node` must be this node), re-validates what it executes (a `run` re-authorizes sudo
/// against THIS node's replicated project row, never the leader's verdict) and never re-proxies: a
/// hop to the wrong node answers `SANDBOX_OWNER_UNREACHABLE`, so the leader learns its metadata is
/// stale instead of chasing a second hop.
#[derive(Deserialize, Serialize)]
pub(crate) struct OwnerOpReq {
    tenant: String,
    project: String,
    sandbox_id: String,
    /// "start" | "restart" | "stop" | "delete" | "run" | "kill"
    op: String,
    #[serde(default)]
    command_id: String,
    #[serde(default)]
    run: Option<RunReq>,
}

pub(crate) async fn owner_op(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<OwnerOpReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_internal_hop(&headers, &claims, &b.tenant)?;
    let rec = c
        .sandboxes
        .get_sandbox(&b.project, &b.sandbox_id)
        .await
        .map_err(sandbox_err)?;
    if crate::admin::norm(&rec.tenant_id) != crate::admin::norm(&b.tenant) {
        return Err(sandbox_err(SandboxError::Unauthorized(
            "sandbox belongs to a different tenant".into(),
        )));
    }
    if rec.owner_node != c.node_name {
        return Err(sandbox_err(SandboxError::OwnerUnreachable(format!(
            "node {} does not own sandbox {} (owner: {})",
            c.node_name,
            rec.id,
            if rec.owner_node.is_empty() {
                "unrecorded"
            } else {
                rec.owner_node.as_str()
            }
        ))));
    }
    let sandbox_id = rec.id.clone();
    match b.op.as_str() {
        "start" => {
            let rec = c
                .sandboxes
                .start_sandbox(&b.project, &sandbox_id)
                .await
                .map_err(sandbox_err)?;
            c.audit
                .record(&b.tenant, "user", "start", "sandbox", &sandbox_id, "");
            crate::persist::persist(&c);
            Ok(Json(json!({ "sandbox": rec })))
        }
        "restart" => {
            c.sandboxes
                .stop_sandbox(&b.project, &sandbox_id)
                .await
                .map_err(sandbox_err)?;
            crate::persist::persist(&c);
            let rec = c
                .sandboxes
                .start_sandbox(&b.project, &sandbox_id)
                .await
                .map_err(sandbox_err)?;
            c.audit
                .record(&b.tenant, "user", "restart", "sandbox", &sandbox_id, "");
            crate::persist::persist(&c);
            Ok(Json(json!({ "sandbox": rec })))
        }
        "stop" => {
            let rec = c
                .sandboxes
                .stop_sandbox(&b.project, &sandbox_id)
                .await
                .map_err(sandbox_err)?;
            c.audit
                .record(&b.tenant, "user", "stop", "sandbox", &sandbox_id, "");
            crate::persist::persist(&c);
            Ok(Json(json!({ "sandbox": rec })))
        }
        "delete" => {
            c.sandboxes
                .delete_sandbox(&b.project, &sandbox_id)
                .await
                .map_err(sandbox_err)?;
            c.audit
                .record(&b.tenant, "user", "delete", "sandbox", &sandbox_id, "");
            crate::persist::persist(&c);
            Ok(Json(json!({ "ok": true })))
        }
        "run" => {
            let run = b.run.ok_or_else(|| bad("run op requires a command body"))?;
            validate_argv(&run.cmd, &run.args).map_err(sandbox_err)?;
            let project_allows_sudo = c.projects.get(&b.project).sandbox_allow_sudo;
            validate_sudo(run.sudo, project_allows_sudo).map_err(sandbox_err)?;
            let input = RunCommandInput {
                cmd: run.cmd,
                args: run.args,
                cwd: run.cwd,
                env: run.env.into_iter().collect(),
                sudo: run.sudo,
                detached: run.detached,
                shell: run.shell,
            };
            let cmd = c
                .sandboxes
                .run_command(&b.project, &sandbox_id, input)
                .await
                .map_err(sandbox_err)?;
            c.audit.record(
                &b.tenant,
                "user",
                "run_command",
                "sandbox_command",
                &cmd.id,
                &format!("{} in {sandbox_id}", cmd.cmd),
            );
            crate::persist::persist(&c);
            Ok(Json(json!({ "command": cmd })))
        }
        "kill" => {
            if b.command_id.is_empty() {
                return Err(bad("kill op requires command_id"));
            }
            let cmd = c
                .sandboxes
                .kill_command(&b.project, &sandbox_id, &b.command_id)
                .await
                .map_err(sandbox_err)?;
            c.audit.record(
                &b.tenant,
                "user",
                "kill_command",
                "sandbox_command",
                &b.command_id,
                "",
            );
            Ok(Json(json!({ "command": cmd })))
        }
        other => Err(bad(&format!("unknown sandbox owner op '{other}'"))),
    }
}

/// Forward one owner op to `owner` and normalize the outcome into the public
/// handler's `Result`: a typed refusal passes through verbatim, a dead
/// transport becomes `SANDBOX_OWNER_UNREACHABLE` (nothing was applied).
async fn forward_owner_op(
    c: &Arc<CloudState>,
    owner: &str,
    team: &str,
    req: &OwnerOpReq,
    budget: Duration,
) -> Result<Value, (StatusCode, String)> {
    match internal_hop(c, owner, OWNER_OP_PATH, team, &json!(req), budget).await {
        Hop::Ok(v) => Ok(v),
        Hop::Refused(status, body) => Err((status, body)),
        Hop::Unreachable(why) => Err(sandbox_err(SandboxError::OwnerUnreachable(format!(
            "sandbox {} is owned by node {owner}, which could not be reached ({why}); nothing was applied — retry shortly",
            req.sandbox_id
        )))),
    }
}

fn owner_reply_field<T: serde::de::DeserializeOwned>(
    v: &Value,
    field: &str,
    owner: &str,
) -> Result<T, (StatusCode, String)> {
    v.get(field)
        .cloned()
        .and_then(|x| serde_json::from_value::<T>(x).ok())
        .ok_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                json!({ "error": format!("owner node {owner} answered without a valid '{field}'") })
                    .to_string(),
            )
        })
}

/// Deletes through the owner when the cell lives elsewhere; the metadata copy adopted here is then
/// forgotten, so the name is free and quotas are right.
async fn delete_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        let req = OwnerOpReq {
            tenant: t.clone(),
            project: project.clone(),
            sandbox_id: sandbox_id.clone(),
            op: "delete".into(),
            command_id: String::new(),
            run: None,
        };
        let v = forward_owner_op(&c, &owner, &t, &req, owner_op_budget()).await?;
        c.sandboxes.forget_record(&project, &sandbox_id);
        c.audit.record(
            &t,
            "user",
            "delete",
            "sandbox",
            &sandbox_id,
            &format!("owner: {owner}"),
        );
        crate::persist::persist(&c);
        return Ok(Json(v));
    }
    c.sandboxes
        .delete_sandbox(&project, &sandbox_id)
        .await
        .map_err(sandbox_err)?;
    c.audit
        .record(&t, "user", "delete", "sandbox", &sandbox_id, "");
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true })))
}

async fn stop_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        let req = OwnerOpReq {
            tenant: t.clone(),
            project: project.clone(),
            sandbox_id: sandbox_id.clone(),
            op: "stop".into(),
            command_id: String::new(),
            run: None,
        };
        let v = forward_owner_op(&c, &owner, &t, &req, owner_op_budget()).await?;
        let rec: SandboxRecord = owner_reply_field(&v, "sandbox", &owner)?;
        c.sandboxes.adopt_record(rec.clone());
        c.audit.record(
            &t,
            "user",
            "stop",
            "sandbox",
            &sandbox_id,
            &format!("owner: {owner}"),
        );
        crate::persist::persist(&c);
        return Ok(Json(json!(masked_sandbox(rec))));
    }
    let rec = c
        .sandboxes
        .stop_sandbox(&project, &sandbox_id)
        .await
        .map_err(sandbox_err)?;
    c.audit
        .record(&t, "user", "stop", "sandbox", &sandbox_id, "");
    crate::persist::persist(&c);
    Ok(Json(json!(masked_sandbox(rec))))
}

/// Starts (or restarts) a stopped sandbox, owner-routed like `stop`; without it a stopped sandbox
/// (a `stop` or node restart tears the cell down) would be terminal. Idempotent: starting a Running
/// sandbox only refreshes its idle deadline. It takes a running slot, so it pays `create`'s quota
/// check, but only when it would add one.
async fn start_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    if let Ok(current) = c.sandboxes.get_sandbox(&project, &sandbox_id).await {
        if current.status != SandboxStatus::Running {
            let plan = plan_of(&c, &t);
            let max_running = crate::billing::plan_max_running_sandboxes(&plan);
            if max_running > 0 {
                let running = c.sandboxes.count_running_for_project(&project);
                if running >= max_running {
                    return Err(sandbox_err(SandboxError::QuotaExceeded(format!(
                        "running-sandbox limit reached ({running}/{max_running}) on the {plan} plan — stop one or upgrade"
                    ))));
                }
            }
        }
    }
    if let Some(rec) = forward_mutation(&c, &project, &sandbox_id, &t, "start").await? {
        return Ok(Json(json!(masked_sandbox(rec))));
    }
    let rec = c
        .sandboxes
        .start_sandbox(&project, &sandbox_id)
        .await
        .map_err(sandbox_err)?;
    c.audit
        .record(&t, "user", "start", "sandbox", &sandbox_id, "");
    crate::persist::persist(&c);
    Ok(Json(json!(masked_sandbox(rec))))
}

/// Restarts a Running sandbox: `start`'s path with the old cell torn down first, for a wedged
/// guest, leaked process or drifted filesystem (`start` is a no-op refresh on a Running record).
/// One hop, on the owner, so a restart can never leave the sandbox stopped because only half of it
/// landed; the stop is persisted before the start, so a failed start cannot leave disk saying
/// Running over a dead cell.
async fn restart_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    if let Some(rec) = forward_mutation(&c, &project, &sandbox_id, &t, "restart").await? {
        return Ok(Json(json!(masked_sandbox(rec))));
    }
    c.sandboxes
        .stop_sandbox(&project, &sandbox_id)
        .await
        .map_err(sandbox_err)?;
    crate::persist::persist(&c);
    let rec = c
        .sandboxes
        .start_sandbox(&project, &sandbox_id)
        .await
        .map_err(sandbox_err)?;
    c.audit
        .record(&t, "user", "restart", "sandbox", &sandbox_id, "");
    crate::persist::persist(&c);
    Ok(Json(json!(masked_sandbox(rec))))
}

#[derive(Deserialize, Serialize)]
struct RunReq {
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    sudo: bool,
    #[serde(default)]
    detached: bool,
    #[serde(default)]
    shell: bool,
}

async fn run_command(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Json(b): Json<RunReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    validate_argv(&b.cmd, &b.args).map_err(sandbox_err)?;
    let project_allows_sudo = c.projects.get(&project).sandbox_allow_sudo;
    validate_sudo(b.sudo, project_allows_sudo).map_err(sandbox_err)?;

    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        let detached = b.detached;
        let req = OwnerOpReq {
            tenant: t.clone(),
            project: project.clone(),
            sandbox_id: sandbox_id.clone(),
            op: "run".into(),
            command_id: String::new(),
            run: Some(b),
        };
        let budget = if detached {
            owner_op_budget()
        } else {
            env_ms("HIVE_SANDBOX_RUN_MS", 110_000)
        };
        let v = forward_owner_op(&c, &owner, &t, &req, budget).await?;
        let cmd: SandboxCommandRecord = owner_reply_field(&v, "command", &owner)?;
        c.audit.record(
            &t,
            "user",
            "run_command",
            "sandbox_command",
            &cmd.id,
            &format!("{} in {sandbox_id} (owner: {owner})", cmd.cmd),
        );
        return Ok(Json(json!(cmd)));
    }

    let input = RunCommandInput {
        cmd: b.cmd,
        args: b.args,
        cwd: b.cwd,
        env: b.env.into_iter().collect(),
        sudo: b.sudo,
        detached: b.detached,
        shell: b.shell,
    };
    let rec = c
        .sandboxes
        .run_command(&project, &sandbox_id, input)
        .await
        .map_err(sandbox_err)?;
    c.audit.record(
        &t,
        "user",
        "run_command",
        "sandbox_command",
        &rec.id,
        &format!("{} in {sandbox_id}", rec.cmd),
    );
    crate::persist::persist(&c);
    Ok(Json(json!(rec)))
}

/// Commands live ONLY on the owner (the leader's adopted copy has none), so a known remote owner is
/// asked first and its failure answers `SANDBOX_OWNER_UNREACHABLE`; the other command reads,
/// `read_file` and `get_domain` route the same way.
pub(crate) async fn list_commands(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let path = format!("/v1/projects/{project}/sandboxes/{sandbox_id}/commands");
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        if let Some(v) = proxy_read(&c, &owner, &path, &t).await {
            return Ok(Json(v));
        }
        return Err(sandbox_err(SandboxError::OwnerUnreachable(format!(
            "sandbox {sandbox_id} is owned by node {owner}, which could not be reached"
        ))));
    }
    match c.sandboxes.list_commands(&project, &sandbox_id).await {
        Ok(list) => Ok(Json(json!({ "commands": list }))),
        Err(e) if is_not_found(&e) => {
            if let Some(node) = read_target(&c, &project, &sandbox_id).await {
                if let Some(v) = proxy_read(&c, &node, &path, &t).await {
                    return Ok(Json(v));
                }
            }
            Err(sandbox_err(e))
        }
        Err(e) => Err(sandbox_err(e)),
    }
}

pub(crate) async fn get_command(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id, command_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let path = format!("/v1/projects/{project}/sandboxes/{sandbox_id}/commands/{command_id}");
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        if let Some(v) = proxy_read(&c, &owner, &path, &t).await {
            return Ok(Json(v));
        }
        return Err(sandbox_err(SandboxError::OwnerUnreachable(format!(
            "sandbox {sandbox_id} is owned by node {owner}, which could not be reached"
        ))));
    }
    match c
        .sandboxes
        .get_command(&project, &sandbox_id, &command_id)
        .await
    {
        Ok(rec) => Ok(Json(json!(rec))),
        Err(e) if is_not_found(&e) => {
            if let Some(node) = read_target(&c, &project, &sandbox_id).await {
                if let Some(v) = proxy_read(&c, &node, &path, &t).await {
                    return Ok(Json(v));
                }
            }
            Err(sandbox_err(e))
        }
        Err(e) => Err(sandbox_err(e)),
    }
}

/// Log streaming, polling-style, like the platform's build-log idiom: returns the command's CURRENT
/// stdout/stderr in full each call and the UI re-fetches on an interval.
pub(crate) async fn get_command_logs(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id, command_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let path = format!("/v1/projects/{project}/sandboxes/{sandbox_id}/commands/{command_id}/logs");
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        if let Some(v) = proxy_read(&c, &owner, &path, &t).await {
            return Ok(Json(v));
        }
        return Err(sandbox_err(SandboxError::OwnerUnreachable(format!(
            "sandbox {sandbox_id} is owned by node {owner}, which could not be reached"
        ))));
    }
    match c
        .sandboxes
        .get_command(&project, &sandbox_id, &command_id)
        .await
    {
        Ok(rec) => Ok(Json(json!({
            "id": rec.id,
            "status": rec.status,
            "exit_code": rec.exit_code,
            "stdout": rec.stdout,
            "stderr": rec.stderr,
            "started_at": rec.started_at,
            "finished_at": rec.finished_at,
        }))),
        Err(e) if is_not_found(&e) => {
            if let Some(node) = read_target(&c, &project, &sandbox_id).await {
                if let Some(v) = proxy_read(&c, &node, &path, &t).await {
                    return Ok(Json(v));
                }
            }
            Err(sandbox_err(e))
        }
        Err(e) => Err(sandbox_err(e)),
    }
}

async fn kill_command(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id, command_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        let req = OwnerOpReq {
            tenant: t.clone(),
            project: project.clone(),
            sandbox_id: sandbox_id.clone(),
            op: "kill".into(),
            command_id: command_id.clone(),
            run: None,
        };
        let v = forward_owner_op(&c, &owner, &t, &req, owner_op_budget()).await?;
        let cmd: SandboxCommandRecord = owner_reply_field(&v, "command", &owner)?;
        c.audit.record(
            &t,
            "user",
            "kill_command",
            "sandbox_command",
            &command_id,
            &format!("owner: {owner}"),
        );
        return Ok(Json(json!(cmd)));
    }
    let rec = c
        .sandboxes
        .kill_command(&project, &sandbox_id, &command_id)
        .await
        .map_err(sandbox_err)?;
    c.audit.record(
        &t,
        "user",
        "kill_command",
        "sandbox_command",
        &command_id,
        "",
    );
    Ok(Json(json!(rec)))
}

#[derive(Deserialize)]
struct ShellQuery {
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}
fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

/// Interactive terminal: a real pty (`vim`/`less`/`^C`/tab-completion), not the polling
/// `commands`/`logs` pair. The live cell is on exactly one node, `SandboxRecord::owner_node` (not
/// always the leader), and a websocket cannot be proxied through `fetch_from_host`'s shape, so the
/// pty only opens on the owner. A client landing on any OTHER node keeps its websocket here and the
/// session is tunneled to the owner over the `RawTarget` mesh surface (`mesh_shell`), because the
/// fleet has no cert-covered per-node hostname to redirect a browser to (a redirect-style
/// `wrong_node` close left terminals permanently "disconnected"). `wrong_node` is sent only when
/// forwarding is impossible: the owner has no iroh address, this node has no mesh, the dial fails,
/// or the OWNER refuses (a target bound to another project, an unsupported protocol version, a
/// shell it could not open; `mesh_shell::resolve_sandbox_shell` logs which): always a typed close,
/// never a silent hang.
///
/// Not-found and unauthorized fail BEFORE the upgrade (an HTTP error beats a socket that opens and
/// closes without a status), and a local miss resolves the record through the read proxy so a third
/// node learns the real owner. Every refusal is logged with its cause, since a refused upgrade is
/// invisible from the browser (a `WebSocket` reports only "connection failed"). An empty
/// `owner_node` is a pre-field record owned by the leader; with no resolvable leader it answers a
/// retryable 503.
async fn open_shell(
    ws: WebSocketUpgrade,
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Query(q): Query<ShellQuery>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let anon = claims.is_none();
    let t = match require(&c, &headers, &claims, &project) {
        Ok(t) => t,
        Err((status, body)) => {
            tracing::info!(
                project = %project,
                sandbox = %sandbox_id,
                status = status.as_u16(),
                anonymous = anon,
                claimed_tenant = %claims.as_ref().map(|e| e.0.tenant.as_str()).unwrap_or(""),
                reason = %body,
                "sandbox shell upgrade refused before the handshake"
            );
            return Err((status, body));
        }
    };

    let rec = match c.sandboxes.get_sandbox(&project, &sandbox_id).await {
        Ok(rec) => rec,
        Err(e) if is_not_found(&e) => {
            let path = format!("/v1/projects/{project}/sandboxes/{sandbox_id}");
            let mut found = None;
            if let Some(node) = read_target(&c, &project, &sandbox_id).await {
                if let Some(v) = proxy_read(&c, &node, &path, &t).await {
                    found = serde_json::from_value::<SandboxRecord>(v).ok();
                }
            }
            match found {
                Some(rec) => rec,
                None => {
                    tracing::info!(
                        project = %project,
                        sandbox = %sandbox_id,
                        tenant = %t,
                        "sandbox shell upgrade refused: no such sandbox on this node, its owner, or the leader"
                    );
                    return Err(sandbox_err(e));
                }
            }
        }
        Err(e) => {
            tracing::info!(project = %project, sandbox = %sandbox_id, tenant = %t, error = ?e, "sandbox shell upgrade refused: record lookup failed");
            return Err(sandbox_err(e));
        }
    };

    let owner = if rec.owner_node.is_empty() {
        let Some(leader) = c.control_plane_leader() else {
            tracing::info!(project = %project, sandbox = %sandbox_id, tenant = %t, "sandbox shell upgrade refused: legacy record owned by the leader, and no leader is resolvable from this node");
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "no control-plane leader is resolvable from this node right now — retry shortly"
                    .into(),
            ));
        };
        leader
    } else {
        rec.owner_node.clone()
    };
    if owner != c.node_name {
        let cols = q.cols;
        let rows = q.rows;
        let mesh = c.mesh.read().clone();
        let addr_json = c
            .registry
            .nodes()
            .into_iter()
            .find(|n| n.name == owner)
            .and_then(|n| n.iroh_addr);
        let (Some(mesh), Some(addr_json)) = (mesh, addr_json) else {
            return Ok(wrong_node_close(
                ws,
                owner,
                "this sandbox's cell is hosted on a different node, and this node has no mesh path to reach it right now; try again shortly",
            ));
        };
        let target = crate::mesh_shell::shell_target(&project, &t, &sandbox_id, cols, rows);
        let raw = match mesh.open_raw_to_port(&owner, &addr_json, &target).await {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!(owner = %owner, sandbox = %sandbox_id, error = %e, "sandbox shell mesh forward failed (target not admitted by the owner: it is not this sandbox's owner, or it runs a build without sandbox-shell forwarding)");
                return Ok(wrong_node_close(
                    ws,
                    owner,
                    "this sandbox's owning node did not accept the terminal forward — it is not this sandbox's owner, or it runs a build without cross-node sandbox-shell forwarding",
                ));
            }
        };
        c.audit.record(
            &t,
            "user",
            "open_shell",
            "sandbox",
            &sandbox_id,
            &format!("owner: {owner} (forwarded over mesh)"),
        );
        return Ok(ws.on_upgrade(move |socket| {
            crate::mesh_shell::bridge_client_side(socket, raw, owner)
        }));
    }

    c.audit
        .record(&t, "user", "open_shell", "sandbox", &sandbox_id, "");

    let (rx, pty) = c
        .sandboxes
        .open_shell(&project, &sandbox_id, q.cols, q.rows)
        .await
        .map_err(sandbox_err)?;
    c.sandboxes.begin_shell_session(&sandbox_id);
    let session = ShellSessionGuard {
        cloud: c.clone(),
        sandbox_id: sandbox_id.clone(),
    };
    Ok(ws.on_upgrade(move |socket| {
        let _session = session;
        pump_shell(socket, rx, pty)
    }))
}

/// Releases `begin_shell_session` (see `open_shell`): an open terminal makes the sandbox BUSY, so
/// the idle reap refreshes its deadline instead of stopping it. Drop-only by design: every exit
/// from the upgrade future (clean close, vanished client, panic) runs it, where an explicit
/// on-close call would miss all but the first.
struct ShellSessionGuard {
    cloud: Arc<CloudState>,
    sandbox_id: String,
}
impl Drop for ShellSessionGuard {
    fn drop(&mut self) {
        self.cloud.sandboxes.end_shell_session(&self.sandbox_id);
    }
}

/// The typed close a client gets when its sandbox's cell is on `owner` and this
/// node cannot tunnel to it: one `wrong_node` control message naming the real
/// owner, then a clean close — never a silent hang or a half-open socket.
fn wrong_node_close(
    ws: WebSocketUpgrade,
    owner: String,
    message: &'static str,
) -> axum::response::Response {
    ws.on_upgrade(move |mut socket| async move {
        let _ = socket
            .send(Message::Text(
                json!({
                    "type": "wrong_node",
                    "owner": owner,
                    "message": message,
                })
                .to_string(),
            ))
            .await;
        let _ = socket.close().await;
    })
}

/// Pumps the owner-local pty over the websocket. Raw keystrokes ALWAYS ride Binary frames; Text
/// frames are control messages (`{"type":"resize","cols":N,"rows":N}`), so a literal `{` typed into
/// the terminal is never parsed as JSON.
async fn pump_shell(
    mut socket: WebSocket,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>,
    pty: hive_backend::PtyIo,
) {
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(hive_core::AgentEvent::PtyOutput { bytes, .. }) => {
                    if socket.send(Message::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                Some(hive_core::AgentEvent::PtyExited { exit_code, .. }) => {
                    let _ = socket
                        .send(Message::Text(
                            json!({ "type": "exited", "exit_code": exit_code }).to_string(),
                        ))
                        .await;
                    break;
                }
                Some(_) => {}
                None => break,
            },
            client = socket.recv() => match client {
                Some(Ok(Message::Binary(bytes))) => pty.input(bytes),
                Some(Ok(Message::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        if v.get("type").and_then(|x| x.as_str()) == Some("resize") {
                            let cols = v.get("cols").and_then(|x| x.as_u64()).unwrap_or(80) as u16;
                            let rows = v.get("rows").and_then(|x| x.as_u64()).unwrap_or(24) as u16;
                            pty.resize(cols, rows);
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
    }
}

#[derive(Deserialize)]
struct WriteFilesReq {
    files: Vec<FileEntry>,
}
#[derive(Deserialize)]
struct FileEntry {
    path: String,
    /// Base64-encoded content (files may be binary).
    content_b64: String,
}

/// Writes execute guest commands, which only the owner can run, and owner routing for this surface
/// is not built (`sandbox-file-io-protocol`): a node whose record names another owner refuses with
/// `SANDBOX_OWNER_UNREACHABLE` rather than failing a local exec.
async fn write_files(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Json(b): Json<WriteFilesReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        return Err(sandbox_err(SandboxError::OwnerUnreachable(format!(
            "sandbox {sandbox_id} is owned by node {owner}; file writes are not yet owner-routed — run commands against the sandbox instead"
        ))));
    }
    use base64::Engine;
    let mut files = Vec::with_capacity(b.files.len());
    for f in b.files {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&f.content_b64)
            .map_err(|e| bad(&format!("invalid base64 for {}: {e}", f.path)))?;
        files.push((f.path, bytes));
    }
    c.sandboxes
        .write_files(&project, &sandbox_id, files)
        .await
        .map_err(sandbox_err)?;
    c.audit
        .record(&t, "user", "write_files", "sandbox", &sandbox_id, "");
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct ReadFileQuery {
    pub(crate) path: String,
}

pub(crate) async fn read_file(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Query(q): Query<ReadFileQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let path = format!(
        "/v1/projects/{project}/sandboxes/{sandbox_id}/files/read?path={}",
        crate::admin::urlencode(&q.path)
    );
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        if let Some(v) = proxy_read(&c, &owner, &path, &t).await {
            return Ok(Json(v));
        }
        return Err(sandbox_err(SandboxError::OwnerUnreachable(format!(
            "sandbox {sandbox_id} is owned by node {owner}, which could not be reached"
        ))));
    }
    match c.sandboxes.read_file(&project, &sandbox_id, &q.path).await {
        Ok(bytes) => {
            use base64::Engine;
            Ok(Json(
                json!({ "path": q.path, "content_b64": base64::engine::general_purpose::STANDARD.encode(bytes) }),
            ))
        }
        Err(e) if is_not_found(&e) => {
            if let Some(node) = read_target(&c, &project, &sandbox_id).await {
                if let Some(v) = proxy_read(&c, &node, &path, &t).await {
                    return Ok(Json(v));
                }
            }
            Err(sandbox_err(e))
        }
        Err(e) => Err(sandbox_err(e)),
    }
}

#[derive(Deserialize, Default)]
struct SnapshotReq {
    #[serde(default, alias = "keepLast")]
    keep_last: Option<u32>,
}

async fn create_snapshot(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Json(b): Json<SnapshotReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let rec = c
        .sandboxes
        .create_snapshot(
            &project,
            &sandbox_id,
            CreateSnapshotInput {
                keep_last: b.keep_last,
            },
        )
        .await
        .map_err(sandbox_err)?;
    c.audit.record(
        &t,
        "user",
        "create_snapshot",
        "sandbox_snapshot",
        &rec.id,
        &sandbox_id,
    );
    crate::persist::persist(&c);
    Ok(Json(json!(rec)))
}

pub(crate) async fn list_snapshots(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    match c.sandboxes.list_snapshots(&project, &sandbox_id).await {
        Ok(list) => Ok(Json(json!({ "snapshots": list }))),
        Err(e) if is_not_found(&e) => {
            let path = format!("/v1/projects/{project}/sandboxes/{sandbox_id}/snapshots");
            if let Some(node) = read_target(&c, &project, &sandbox_id).await {
                if let Some(v) = proxy_read(&c, &node, &path, &t).await {
                    return Ok(Json(v));
                }
            }
            Err(sandbox_err(e))
        }
        Err(e) => Err(sandbox_err(e)),
    }
}

async fn delete_snapshot(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, _sandbox_id, snapshot_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    c.sandboxes
        .delete_snapshot(&project, &snapshot_id)
        .await
        .map_err(sandbox_err)?;
    c.audit.record(
        &t,
        "user",
        "delete_snapshot",
        "sandbox_snapshot",
        &snapshot_id,
        "",
    );
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct MountReq {
    #[serde(alias = "mountPath")]
    mount_path: String,
    #[serde(rename = "type")]
    kind: String,
    mode: String,
    provider: String,
    #[serde(default)]
    bucket: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    endpoint: String,
    #[serde(default, alias = "accessKey")]
    access_key: String,
    #[serde(default, alias = "secretKey")]
    secret_key: String,
    #[serde(default, alias = "extraArgs")]
    extra_args: HashMap<String, String>,
}

async fn create_mount(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Json(b): Json<MountReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let plan = plan_of(&c, &t);
    let max_mounts = crate::billing::plan_max_sandbox_mounts(&plan);
    if max_mounts > 0 {
        let count = c
            .sandboxes
            .list_mounts(&project, &sandbox_id)
            .await
            .map_err(sandbox_err)?
            .len() as u32;
        if count >= max_mounts {
            return Err(sandbox_err(SandboxError::QuotaExceeded(format!("mount limit reached ({count}/{max_mounts}) on the {plan} plan — upgrade to add more"))));
        }
    }
    let input = MountConfigInput {
        mount_path: b.mount_path,
        kind: b.kind,
        mode: b.mode,
        provider: b.provider,
        bucket: b.bucket,
        region: b.region,
        endpoint: b.endpoint,
        access_key: b.access_key,
        secret_key: b.secret_key,
        extra_args: b.extra_args,
    };
    let rec = c
        .sandboxes
        .mount_storage(&project, &sandbox_id, input)
        .await
        .map_err(sandbox_err)?;
    c.audit
        .record(&t, "user", "mount", "sandbox_mount", &rec.id, &sandbox_id);
    crate::persist::persist(&c);
    Ok(Json(json!(masked_mount(rec))))
}

fn masked_mount(mut m: SandboxMountRecord) -> SandboxMountRecord {
    for k in ["access_key_enc", "secret_key_enc"] {
        if m.config_refs.contains_key(k) {
            m.config_refs.insert(k.to_string(), "••••••••".to_string());
        }
    }
    m
}

pub(crate) async fn list_mounts(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    match c.sandboxes.list_mounts(&project, &sandbox_id).await {
        Ok(list) => Ok(Json(
            json!({ "mounts": list.into_iter().map(masked_mount).collect::<Vec<_>>() }),
        )),
        Err(e) if is_not_found(&e) => {
            let path = format!("/v1/projects/{project}/sandboxes/{sandbox_id}/mounts");
            if let Some(node) = read_target(&c, &project, &sandbox_id).await {
                if let Some(v) = proxy_read(&c, &node, &path, &t).await {
                    return Ok(Json(v));
                }
            }
            Err(sandbox_err(e))
        }
        Err(e) => Err(sandbox_err(e)),
    }
}

async fn delete_mount(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, _sandbox_id, mount_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    c.sandboxes
        .delete_mount(&project, &mount_id)
        .await
        .map_err(sandbox_err)?;
    c.audit
        .record(&t, "user", "unmount", "sandbox_mount", &mount_id, "");
    crate::persist::persist(&c);
    Ok(Json(json!({ "ok": true })))
}

async fn update_network_policy(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Json(policy): Json<NetworkPolicy>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let rec = c
        .sandboxes
        .update_network_policy(&project, &sandbox_id, policy)
        .await
        .map_err(sandbox_err)?;
    c.audit.record(
        &t,
        "user",
        "update_network_policy",
        "sandbox",
        &sandbox_id,
        &rec.network_policy.mode,
    );
    crate::persist::persist(&c);
    Ok(Json(json!(masked_sandbox(rec))))
}

#[derive(Deserialize)]
pub(crate) struct DomainQuery {
    pub(crate) port: u16,
}

/// The preview URL names the node hosting the cell, so a known remote owner answers it with ITS
/// address.
pub(crate) async fn get_domain(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Query(q): Query<DomainQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let path = format!(
        "/v1/projects/{project}/sandboxes/{sandbox_id}/domain?port={}",
        q.port
    );
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        if let Some(v) = proxy_read(&c, &owner, &path, &t).await {
            return Ok(Json(v));
        }
        return Err(sandbox_err(SandboxError::OwnerUnreachable(format!(
            "sandbox {sandbox_id} is owned by node {owner}, which could not be reached"
        ))));
    }
    match c.sandboxes.domain(&project, &sandbox_id, q.port).await {
        Ok(url) => Ok(Json(json!({ "url": url, "port": q.port }))),
        Err(e) if is_not_found(&e) => {
            if let Some(node) = read_target(&c, &project, &sandbox_id).await {
                if let Some(v) = proxy_read(&c, &node, &path, &t).await {
                    return Ok(Json(v));
                }
            }
            Err(sandbox_err(e))
        }
        Err(e) => Err(sandbox_err(e)),
    }
}
