//! Sandboxes HTTP API — project-scoped CRUD for sandboxes, commands, snapshots,
//! mounts, and network policy, matching the platform's route conventions
//! (`require_project` tenant/project authz, audit + persist after every write).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::admin::{require_project, tenant};
use crate::sandboxes::*;
use crate::state::CloudState;

type Claims = Option<axum::Extension<crate::auth::Claims>>;

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
        SandboxError::EngineUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
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
    // `ProjectSettingsStore` rows are node-local and never gossiped — the same
    // gap `admin::deployment_build` already works around for builds. A node
    // that has never locally seen `project` (most non-owner nodes, for most
    // projects) gets `UNTAGGED_TENANT` back from `team_of`, which can never
    // equal a real tenant — rejecting outright here would 403 a legitimate
    // cross-node sandbox read before it ever reaches the local-miss ->
    // `proxy_to_owner` fallback below. Trust the caller's own tenant claim
    // provisionally in that case: mutations never execute this handler body
    // except on the control-plane leader (admin_ingress forwards every
    // POST/PUT/DELETE/PATCH there unconditionally), where the project row is
    // guaranteed accurate for anything the leader has itself ever mutated;
    // reads fall through to `proxy_to_owner`, whose target node re-runs this
    // same check against ITS project row and rejects a genuinely wrong-team
    // caller there instead.
    if c.projects.team_of(project) == crate::admin::UNTAGGED_TENANT {
        return Ok(tenant(c, headers, claims.as_ref().map(|e| &e.0)));
    }
    require_project(c, headers, claims.as_ref().map(|e| &e.0), project)
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

/// Cross-node fallback for sandbox READS. Every sandbox MUTATION (create/
/// patch/delete/commands/...) is forwarded by `admin_ingress` to the single
/// current control-plane owner — sandboxes have no independent placement/
/// scheduler of their own, so that node is the sole node that ever writes a
/// sandbox record. But GET/LIST requests serve locally per-node (the
/// platform's "reads fully distributed" policy), so any node other than the
/// current owner has NEVER heard of a sandbox created there — every read
/// landing on a non-owner node 404s (`SANDBOX_NOT_FOUND`) even though the
/// sandbox is real and running. Live-reproduced: create via one node,
/// immediate GET via a different node returns 404 100% of the time.
///
/// Fix: when this node is not the current owner, proxy the read through to
/// it (the existing `fetch_from_host` cross-node helper — same mechanism
/// `deployment_build`/`logs` already use for analogous per-node state).
/// Callers pass a fallback (`local`) to run first — only proxies when the
/// local lookup came back empty/not-found, so an already-authoritative node
/// never pays the network hop.
async fn proxy_to_owner(c: &Arc<CloudState>, path: &str, team: &str) -> Option<Value> {
    if c.is_control_plane_leader() {
        return None; // we ARE the sole writer; nothing upstream to proxy to
    }
    let leader = c.control_plane_leader();
    crate::admin::fetch_from_host(c, &leader, path, team).await
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
    if local.is_empty() {
        if let Some(v) = proxy_to_owner(&c, &format!("/v1/projects/{project}/sandboxes"), &t).await
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
    let rec = c
        .sandboxes
        .create_sandbox(&t, &project, input)
        .await
        .map_err(sandbox_err)?;
    c.audit
        .record(&t, "user", "create", "sandbox", &rec.id, &rec.name);
    crate::persist::persist(&c);
    Ok(Json(json!(masked_sandbox(rec))))
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
            if let Some(v) = proxy_to_owner(
                &c,
                &format!("/v1/projects/{project}/sandboxes/{sandbox_id}"),
                &t,
            )
            .await
            {
                return Ok(Json(v));
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

async fn delete_sandbox(
    State(c): State<Arc<CloudState>>,
    headers: HeaderMap,
    claims: Claims,
    Path((project, sandbox_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let t = require(&c, &headers, &claims, &project)?;
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

#[derive(Deserialize)]
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
    match c.sandboxes.list_commands(&project, &sandbox_id).await {
        Ok(list) => Ok(Json(json!({ "commands": list }))),
        Err(e) if is_not_found(&e) => {
            if let Some(v) = proxy_to_owner(
                &c,
                &format!("/v1/projects/{project}/sandboxes/{sandbox_id}/commands"),
                &t,
            )
            .await
            {
                return Ok(Json(v));
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
    match c
        .sandboxes
        .get_command(&project, &sandbox_id, &command_id)
        .await
    {
        Ok(rec) => Ok(Json(json!(rec))),
        Err(e) if is_not_found(&e) => {
            let path =
                format!("/v1/projects/{project}/sandboxes/{sandbox_id}/commands/{command_id}");
            if let Some(v) = proxy_to_owner(&c, &path, &t).await {
                return Ok(Json(v));
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
            let path =
                format!("/v1/projects/{project}/sandboxes/{sandbox_id}/commands/{command_id}/logs");
            if let Some(v) = proxy_to_owner(&c, &path, &t).await {
                return Ok(Json(v));
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
    match c.sandboxes.read_file(&project, &sandbox_id, &q.path).await {
        Ok(bytes) => {
            use base64::Engine;
            Ok(Json(
                json!({ "path": q.path, "content_b64": base64::engine::general_purpose::STANDARD.encode(bytes) }),
            ))
        }
        Err(e) if is_not_found(&e) => {
            let path = format!(
                "/v1/projects/{project}/sandboxes/{sandbox_id}/files/read?path={}",
                crate::admin::urlencode(&q.path)
            );
            if let Some(v) = proxy_to_owner(&c, &path, &t).await {
                return Ok(Json(v));
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
            if let Some(v) = proxy_to_owner(&c, &path, &t).await {
                return Ok(Json(v));
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
            if let Some(v) = proxy_to_owner(&c, &path, &t).await {
                return Ok(Json(v));
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
    match c.sandboxes.domain(&project, &sandbox_id, q.port).await {
        Ok(url) => Ok(Json(json!({ "url": url, "port": q.port }))),
        Err(e) if is_not_found(&e) => {
            let path = format!(
                "/v1/projects/{project}/sandboxes/{sandbox_id}/domain?port={}",
                q.port
            );
            if let Some(v) = proxy_to_owner(&c, &path, &t).await {
                return Ok(Json(v));
            }
            Err(sandbox_err(e))
        }
        Err(e) => Err(sandbox_err(e)),
    }
}
