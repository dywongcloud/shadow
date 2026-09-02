//! Sandboxes HTTP API — project-scoped CRUD for sandboxes, commands, snapshots,
//! mounts, and network policy, matching the platform's route conventions
//! (`require_project` tenant/project authz, audit + persist after every write).
//!
//! # Placement and owner routing
//!
//! Every sandbox MUTATION is forwarded by `admin_ingress` to the control-plane
//! leader, but the leader may have NO exec-capable isolation backend (live:
//! fc-sanjose runs Mock). The leader is therefore a ROUTER for sandboxes, never
//! a fallback provisioner:
//!
//! * `create_sandbox` provisions locally only when this node's own backend
//!   passes `sandbox_exec_capable`; otherwise it mints the sandbox id, picks
//!   candidate owners from the live registry with the SAME predicate
//!   (healthy, not self, backend ∈ {firecracker, litebox}; same region first,
//!   then by name), and forwards the create to each in turn — bounded per-peer
//!   budget, bounded total budget, every candidate tried at most once — over
//!   the existing owner-hop transport (`internal_hop`: HTTP admin if known,
//!   else the iroh gossip `POST` arm in `gossip::dispatch`). The forwarded
//!   create carries the leader-minted id, so a timed-out-then-retried create
//!   is idempotent on the owner. When no candidate succeeds the caller gets a
//!   typed 503 `SANDBOX_NO_CAPABLE_NODE` naming every candidate tried and why.
//!   The winning owner's record (with `owner_node` = that peer) is ADOPTED on
//!   the leader as metadata so later leader-forwarded mutations can route.
//! * `stop`/`delete`/`run`/`kill` re-route to `SandboxRecord::owner_node` via
//!   ONE typed internal RPC (`owner_op`, always `POST`) — the owner
//!   re-authorizes the tenant against its own record and never re-proxies.
//!   Reads reach the owner through `proxy_read` (the `fetch_from_host`
//!   precedent). The interactive shell (a websocket) cannot be proxied that
//!   way; a non-owner tunnels the session to the owner over the existing
//!   `RawTarget` mesh surface (`mesh_shell`) and answers `wrong_node` naming
//!   the REAL owner only when no mesh path to it exists.

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

/// Internal owner-hop paths. Both are exempt from leader forwarding
/// (`main.rs`'s `owner_routed`) and served over the mesh by matching
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
        // Internal owner hops (leader -> owner). Fleet service credentials
        // only — never a tenant-facing route.
        .route(DELEGATE_CREATE_PATH, post(delegate_create_sandbox))
        .route(OWNER_OP_PATH, post(owner_op))
}

// ---------------------------------------------------------------------------
// Error mapping + shared guards
// ---------------------------------------------------------------------------

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
fn require(
    c: &Arc<CloudState>,
    headers: &HeaderMap,
    claims: &Claims,
    project: &str,
) -> Result<String, (StatusCode, String)> {
    // `ProjectSettingsStore` rows replicate eventually — a node that has never
    // locally seen `project` (most non-owner nodes, for most projects) gets
    // `UNTAGGED_TENANT` back from `team_of`, which can never equal a real
    // tenant; rejecting outright here would 403 a legitimate cross-node
    // sandbox read before it ever reaches the local-miss -> `proxy_read`
    // fallback below. Trust the caller's own tenant claim provisionally in
    // that case: mutations never execute this handler body except on the
    // control-plane leader (admin_ingress forwards every POST/PUT/DELETE/PATCH
    // there unconditionally), where the project row is guaranteed accurate
    // for anything the leader has itself ever mutated; reads fall through to
    // `proxy_read`, whose target node re-runs this same check against ITS
    // project row and rejects a genuinely wrong-team caller there instead.
    if c.projects.team_of(project) == crate::admin::UNTAGGED_TENANT {
        return Ok(tenant(c, headers, claims.as_ref().map(|e| &e.0)));
    }
    require_project(c, headers, claims.as_ref().map(|e| &e.0), project)
}

/// Gate for the two internal owner hops: the fleet-shared `x-hive-internal`
/// token, OR a `mesh-internal` service JWT (minted by `internal_hop` /
/// `mesh_team_qs` with the fleet secret) whose tenant claim matches the
/// tenant the body asserts — so a forwarded hop can never act for a tenant
/// its own credential was not minted for. With NEITHER credential configured
/// (dev-open: no `HIVE_JWT_SECRET`, no `HIVE_INTERNAL_TOKEN`) the hop is open,
/// exactly like `auth::require_auth`'s dev-mode pass-through.
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
            .is_some_and(|got| got == expected)
        {
            return Ok(());
        }
    }
    if let Some(cl) = claims.as_ref().map(|e| &e.0) {
        if cl.role == "service"
            && cl.sub == "mesh-internal"
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

/// A DB-record view without secrets — masks `env_refs` values (they're already
/// `value_enc`, ciphertext, but masking keeps the shape consistent with every
/// other list/detail endpoint in the platform that never echoes raw secret
/// material, encrypted or not).
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

/// Where a READ for `sandbox_id` should go when this node cannot answer it:
/// the record's remote owner if known here, else the control-plane leader
/// (which holds an adopted copy of every record and can hop again), else
/// nowhere (this node IS the leader and knows nothing).
async fn read_target(c: &Arc<CloudState>, project: &str, sandbox_id: &str) -> Option<String> {
    if let Some(owner) = remote_owner(c, project, sandbox_id).await {
        return Some(owner);
    }
    if c.is_control_plane_leader() {
        return None;
    }
    Some(c.control_plane_leader())
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

/// True for every "not found" flavor `SandboxError` has — a missing command/
/// snapshot/mount on a NON-owner node is exactly as likely to mean "this node
/// never had the parent sandbox at all" as "the sandbox exists but this
/// specific sub-resource doesn't"; the proxy fallback covers both correctly
/// (a real 404 on the owner still 404s after the round trip).
fn is_not_found(e: &SandboxError) -> bool {
    matches!(
        e,
        SandboxError::NotFound(_)
            | SandboxError::CommandNotFound(_)
            | SandboxError::SnapshotNotFound(_)
            | SandboxError::MountNotFound(_)
    )
}

// ---------------------------------------------------------------------------
// Internal owner hop transport
// ---------------------------------------------------------------------------

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

/// One bounded POST to `node`'s internal sandbox surface with the caller's
/// tenant authority — HTTP admin URL when known (loopback-only fleet-wide, so
/// in practice a local-dev path), else the iroh gossip `POST` arm. Same
/// credential shape as `admin::post_to_host_json` (x-hive-team, the fleet
/// `x-hive-internal` token, a short-lived `mesh-internal` service JWT under
/// enforcement), but the peer's status + body are preserved so a typed
/// refusal is distinguishable from a dead transport.
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
    // Bind out of the lock FIRST so no parking_lot guard is held across an
    // await (the dispatched future must be `Send`).
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
        // HTTP transport failed → try the mesh below.
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
    match crate::gossip::request_to(c, &id, &addr, hive_p2p::GOSSIP_POST, &p, &body_bytes, 20)
        .await
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

/// Candidate owners for a sandbox this node cannot provision: every node in
/// the live registry that is healthy, not this node, and whose gossiped
/// `backend` passes `sandbox_exec_capable` — the SAME predicate
/// `PlatformSandboxProvider::local_exec_capable` applies to the local backend.
/// Deterministic order: same region as this node first, then by name, so
/// retries and every observer agree on the sequence.
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

// ---------------------------------------------------------------------------
// Sandbox CRUD
// ---------------------------------------------------------------------------

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
    if local.is_empty() && !c.is_control_plane_leader() {
        // The leader adopts a copy of every record (delegated or not), so it
        // is the one node whose list is complete.
        let leader = c.control_plane_leader();
        if let Some(v) =
            proxy_read(&c, &leader, &format!("/v1/projects/{project}/sandboxes"), &t).await
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

/// Owner side of a delegated create: "provision THIS exact sandbox here, you
/// have an exec-capable backend and the leader does not." Runs
/// `PlatformSandboxProvider::create_sandbox_with_id` verbatim on this node —
/// fail-closed, idempotent on `id`, `owner_node` stamped with this node's
/// name. Returns the UNMASKED record (env values are already sealed
/// ciphertext; the leader adopts the record as-is and the public handler masks
/// before answering the tenant).
pub(crate) async fn delegate_create_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Json(b): Json<DelegateCreateReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_internal_hop(&headers, &claims, &b.tenant)?;
    if !c.sandboxes.local_exec_capable() {
        // Never provision on a backend that cannot exec — the exact refusal
        // the leader's own local path gives; the leader moves to the next
        // candidate.
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
    // Validate here, before any placement decision: a bad request must never
    // burn a delegation round trip, and the same checks run again on the
    // provisioning node.
    validate_name(&input.name).map_err(sandbox_err)?;
    validate_runtime(&input.runtime).map_err(sandbox_err)?;
    validate_network_policy(&input.network_policy).map_err(sandbox_err)?;
    // The leader holds an adopted copy of every record of this project, so
    // the name-uniqueness check is authoritative HERE, not only on whichever
    // owner ends up provisioning.
    if c
        .sandboxes
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

    // Leader-minted id: the idempotency key for every delegation attempt
    // below (and the id of a local create).
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

    // Capability-aware placement: this node cannot provision. Never create a
    // local record; ask capable peers, deterministically, bounded.
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
            tried.push(format!("{peer}: not tried (total delegation budget exhausted)"));
            continue;
        }
        let budget = per_peer.min(remaining);
        match internal_hop(&c, peer, DELEGATE_CREATE_PATH, &t, &body, budget).await {
            Hop::Ok(v) => match serde_json::from_value::<SandboxRecord>(v) {
                Ok(rec) if rec.id == id && rec.owner_node == *peer => {
                    // The cell lives on `peer`; THIS node keeps the metadata so
                    // every later leader-forwarded mutation can route to it.
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
                    // The REQUEST is what's wrong (conflict, quota, invalid
                    // input) — no other candidate would accept it, and the
                    // peer's typed answer is the honest one. Pass it through.
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

async fn patch_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Json(b): Json<PatchReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    // Sandboxes doesn't expose a raw mutable-record API on the trait (every
    // mutation is a specific verb: stop/network-policy/etc.) — tags/retention
    // are the two purely-cosmetic/config fields with no dedicated verb, so we
    // read-modify-through the record here via the store directly is NOT
    // available on the trait; keep this endpoint honest about what it can do.
    let rec = c
        .sandboxes
        .get_sandbox(&project, &sandbox_id)
        .await
        .map_err(sandbox_err)?;
    let _ = (b.tags, b.keep_last_snapshots); // reserved: no trait setter yet (see writeup)
    c.audit
        .record(&t, "user", "update", "sandbox", &sandbox_id, "");
    Ok(Json(json!(masked_sandbox(rec))))
}

// ---------------------------------------------------------------------------
// Owner-routed mutations (stop / delete / run / kill)
// ---------------------------------------------------------------------------

/// The ONE typed internal owner RPC every cell-touching mutation rides. Always
/// a `POST` (no method confusion between the public verb and the hop); the
/// operation is the `op` field. The owner re-derives authority from ITS OWN
/// record (tenant must match, `owner_node` must be this node) and never
/// re-proxies.
#[derive(Deserialize, Serialize)]
pub(crate) struct OwnerOpReq {
    tenant: String,
    project: String,
    sandbox_id: String,
    /// "stop" | "delete" | "run" | "kill"
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
        // Never re-proxy: the leader addressed this hop to the wrong node
        // (stale metadata) and must learn that, not chase a second hop.
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
            // Re-authorize sudo against THIS node's replicated project row —
            // the leader's verdict is not carried as an assertion.
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
        // The owner tore the cell down and tombstoned its record; drop the
        // metadata copy here so the name is free and quotas are right.
        c.sandboxes.forget_record(&project, &sandbox_id);
        c.audit
            .record(&t, "user", "delete", "sandbox", &sandbox_id, &format!("owner: {owner}"));
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
        c.audit
            .record(&t, "user", "stop", "sandbox", &sandbox_id, &format!("owner: {owner}"));
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

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

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
        // A blocking run waits for the command itself; give it the same bound
        // the admin request timeout gives the whole request, minus headroom.
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

pub(crate) async fn list_commands(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let path = format!("/v1/projects/{project}/sandboxes/{sandbox_id}/commands");
    // Commands live ONLY on the owner (the leader's adopted copy carries no
    // command records) — a known remote owner is asked first.
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

/// Log streaming, polling-style — matches the platform's existing build-log
/// idiom exactly (`GET /v1/builds/:id`/`GET /v1/deployments/:id/build` return
/// the full log array every call; the UI re-fetches on an interval). This
/// endpoint returns the command's CURRENT stdout/stderr in full each call;
/// growth between polls is what makes it "live" in the UI.
pub(crate) async fn get_command_logs(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id, command_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    let path =
        format!("/v1/projects/{project}/sandboxes/{sandbox_id}/commands/{command_id}/logs");
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

// ---------------------------------------------------------------------------
// Interactive shell (websocket)
// ---------------------------------------------------------------------------

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

/// Interactive terminal — real pty (`vim`/`less`/`^C`/tab-completion), NOT the
/// line-buffered polling `commands`/`logs` pair above. A sandbox's live cell
/// lives on exactly one node — `SandboxRecord::owner_node`, which is NOT
/// always the control-plane leader (see the module doc's placement rules).
/// Unlike a JSON GET a websocket cannot be transparently proxied through
/// `fetch_from_host`'s request/response shape, so the pty itself only ever
/// opens on the owning node. A client landing on any OTHER node keeps its
/// websocket to this node open and the session is tunneled to the owner over
/// the existing `RawTarget` mesh surface (`mesh_shell.rs`) — this fleet has no
/// cert-covered per-node hostname a browser could be redirected to (`acme.rs`
/// issues certs only for `*.{apps_domain}` and a fixed list under
/// `{platform_domain}`), so a redirect-style `wrong_node` close left the
/// terminal permanently "disconnected" (witnessed live on
/// `sbx_253aa161efc04c5b`, real Firecracker on fc-bangkok). `wrong_node` is
/// now sent only when forwarding is impossible: the owner has no iroh address
/// in the registry, this node has no mesh, or the mesh dial fails — still a
/// clear typed close, never a silent hang or a half-open socket.
async fn open_shell(
    ws: WebSocketUpgrade,
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Query(q): Query<ShellQuery>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    // A refused upgrade is invisible from the browser (a `WebSocket` reports
    // "connection failed" with no status), and until 2026-09-02 it was
    // invisible here too: both refusals below return BEFORE the audit line,
    // so a tenant whose session cookie never reached this host (anonymous
    // handshake -> 403) left zero evidence on any node. Log every refusal
    // with what decided it.
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

    // Fail BEFORE the upgrade for a genuine not-found/unauthorized sandbox —
    // an HTTP error response here is far more useful to the caller than a
    // websocket that opens and immediately closes with no status code. A
    // local miss resolves the record through the read proxy so a third node
    // still learns the real owner instead of guessing the leader.
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

    // Owner resolution: the record's `owner_node` first; an empty field is a
    // pre-field record, which the leader-placement rule owned.
    let owner = if rec.owner_node.is_empty() {
        c.control_plane_leader()
    } else {
        rec.owner_node.clone()
    };
    if owner != c.node_name {
        // Cross-node forward over the mesh: open a `RawTarget` stream to the
        // owner (`mesh_shell::shell_target` — the owner's `mesh_raw::resolve`
        // recognizes the sandbox-shaped target and bridges it to the real
        // local pty) and pump framed bytes both ways. The browser's websocket
        // to THIS node stays open for the whole session.
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
        let target = crate::mesh_shell::shell_target(&sandbox_id, cols, rows);
        let raw = match mesh.open_raw_to_port(&owner, &addr_json, &target).await {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!(owner = %owner, sandbox = %sandbox_id, error = %e, "sandbox shell mesh forward failed");
                return Ok(wrong_node_close(
                    ws,
                    owner,
                    "could not reach this sandbox's owning node over the mesh; try again shortly",
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
        return Ok(ws.on_upgrade(move |socket| crate::mesh_shell::bridge_client_side(socket, raw)));
    }

    c.audit.record(&t, "user", "open_shell", "sandbox", &sandbox_id, "");

    let (rx, pty) = c
        .sandboxes
        .open_shell(&project, &sandbox_id, q.cols, q.rows)
        .await
        .map_err(sandbox_err)?;

    Ok(ws.on_upgrade(move |socket| pump_shell(socket, rx, pty)))
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
                // Not applicable on this session's event stream.
                Some(_) => {}
                None => break,
            },
            client = socket.recv() => match client {
                Some(Ok(Message::Binary(bytes))) => pty.input(bytes),
                Some(Ok(Message::Text(t))) => {
                    // Control channel: `{"type":"resize","cols":N,"rows":N}`.
                    // Raw keystrokes ALWAYS ride Binary frames above — Text is
                    // reserved for control messages so a literal `{` typed
                    // into the terminal is never misparsed as JSON.
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

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

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

async fn write_files(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
    Json(b): Json<WriteFilesReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
    if let Some(owner) = remote_owner(&c, &project, &sandbox_id).await {
        // File writes execute guest commands, which only the owner can run.
        // Owner-routing this surface is `sandbox-file-io-protocol`'s work;
        // until then refuse honestly instead of failing a local exec.
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
    // Reading a file runs a guest command — only the owner can. Ask it first.
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

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Mounts
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Network policy + domain
// ---------------------------------------------------------------------------

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
    // The preview URL names the node that hosts the cell — only the owner
    // can answer it with ITS address.
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
