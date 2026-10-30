//! `PlatformSandboxProvider`, the production [`SandboxProvider`]: every sandbox is a long-lived
//! cell of the generic `hive_backend::CellBackend`, a Firecracker microVM or, where microVMs are
//! unavailable or suppressed and Litebox is verified (`HIVE_LITEBOX_VERIFIED=1`), a Litebox cell;
//! `main.rs` ranks the shared backend the same way. Never Mock: a node with neither reports
//! `EngineUnavailable` instead of presenting a fake sandbox that isolates nothing.
//!
//! A cell lives until `stop`, `delete` or the idle timeout. Commands ride the guest-agent
//! protocol's `AgentRequest::Exec`/`KillExec` (`hive-core/proto.rs`): one argv command per call
//! over its own vsock connection, streaming `ExecOutput` (stdout and stderr kept distinct) and then
//! one `ExecDone`. The interactive terminal rides
//! `ExecPty`/`PtyInput`/`PtyResize`/`PtyOutput`/`PtyExited` (`CellBackend::exec_command`,
//! `kill_exec`, `exec_pty`).
//!
//! `create_sandbox` FAILS CLOSED: a failed provision persists no record, and a partially
//! provisioned cell is released by a Drop guard on the error and cancel paths. There is no
//! simulation mode: a sandbox either has a real cell on `SandboxRecord::owner_node` or does not
//! exist. This provider only ever provisions locally; placing a create on a capable node is
//! `sandboxes_api::create_sandbox`'s job (it delegates from an incapable leader to a capable peer).

use crate::sandboxes::*;
use hive_backend::{CellBackend, CellHandle, CellSpec};
use hive_core::{CellId, ExecRequest as GuestExecRequest, ResourceSpec};
use parking_lot::RwLock as PLRwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Holds a freshly provisioned cell between `provision` returning and the sandbox record being
/// committed. Armed by construction: if the create path errors or the request future is dropped
/// (axum drops it when the client goes away) before `commit`, the cell is terminated instead of
/// leaking as an untracked microVM or litebox process. `commit` publishes the handle into the
/// provider's live `cells` map and disarms.
struct ProvisionedCellGuard {
    sandbox_id: String,
    handle: Option<CellHandle>,
    backend: Arc<dyn CellBackend>,
    cells: Arc<PLRwLock<HashMap<String, CellHandle>>>,
}

impl ProvisionedCellGuard {
    fn commit(mut self) -> CellHandle {
        let handle = self
            .handle
            .take()
            .expect("ProvisionedCellGuard::commit called twice");
        self.cells
            .write()
            .insert(self.sandbox_id.clone(), handle.clone());
        handle
    }
}

impl Drop for ProvisionedCellGuard {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let backend = self.backend.clone();
        let sandbox_id = self.sandbox_id.clone();
        tracing::warn!(
            sandbox = %sandbox_id,
            cell = %handle.id,
            "releasing a provisioned sandbox cell whose record was never committed (create failed or was cancelled)"
        );
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = backend.terminate(&handle).await {
                    tracing::warn!(sandbox = %sandbox_id, %error, "uncommitted sandbox cell teardown failed");
                }
            });
        }
    }
}

/// Mints a platform sandbox id: `sbx_` + 16 lowercase hex. The leader mints it BEFORE any
/// transport, so a delegated create that times out and is retried carries the same id, which the
/// owner treats as the same request.
pub fn mint_sandbox_id() -> String {
    format!("sbx_{}", &uuid::Uuid::new_v4().simple().to_string()[..16])
}

/// Host programs a sandbox shell needs beyond the backend's fixed sets (`SHELL_GUEST_PROGRAMS`, and
/// the `python3`/`git`/`curl`/`wget` toolchain `SHELL_GUEST_TOOL_PROGRAMS`): the interpreter of the
/// sandbox's runtime family. Naming just the interpreter is enough, because the litebox backend's
/// `support_plan_for` recognizes `node`/`python3` by their resolved binary name and stages their
/// whole standard-library and package-manager tree alongside (npm's `node_modules_22`, Python's
/// stdlib). `npm`/`npx` are never listed separately, so a python-runtime shell does not carry the
/// npm tree.
fn shell_runtime_programs(runtime: &str) -> Vec<String> {
    if runtime.starts_with("node") {
        vec!["node".to_string()]
    } else if runtime.starts_with("python") {
        vec!["python3".to_string()]
    } else {
        Vec::new()
    }
}

fn runtime_image(runtime: &str) -> &'static str {
    match runtime {
        "node26" => "sandbox-node26",
        "node24" => "sandbox-node24",
        "node22" => "sandbox-node22",
        "python3.13" => "sandbox-python313",
        _ => "sandbox-node22",
    }
}

const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
/// Distinct from Fluid function cells and build cells (their own `CellId`
/// namespaces) purely by prefix — cells are otherwise identical infrastructure.
fn cell_id_for(sandbox_id: &str) -> CellId {
    CellId::from(format!("sbx-{sandbox_id}"))
}

pub struct PlatformSandboxProvider {
    sandboxes: Arc<PLRwLock<Vec<SandboxRecord>>>,
    commands: Arc<PLRwLock<Vec<SandboxCommandRecord>>>,
    snapshots: Arc<PLRwLock<Vec<SandboxSnapshotRecord>>>,
    mounts: Arc<PLRwLock<Vec<SandboxMountRecord>>>,
    /// Live cell handles, NOT persisted: a `CellHandle`'s vsock/socket paths do not survive a node
    /// restart, so a running sandbox is re-provisioned fresh on its next use.
    cells: Arc<PLRwLock<HashMap<String, CellHandle>>>,
    /// Live kill-channels for detached execs, keyed by command id, so
    /// `kill_command` can stop draining even if the guest-side kill races.
    killers: Arc<PLRwLock<HashMap<String, Arc<tokio::sync::Notify>>>>,
    /// Sandbox ids with an interactive SHELL session open on this node right now. A terminal
    /// someone is typing in is not idle, so the idle-timeout reap refreshes its deadline instead of
    /// tearing the cell down under it. Released by a `Drop` guard in the websocket handler, never
    /// by the client's close frame alone (a vanished client drops the upgrade future and never
    /// sends one).
    open_shells: Arc<PLRwLock<HashSet<String>>>,
    backend: Option<Arc<dyn CellBackend>>,
    region: String,
    /// This node's own name: stamped onto `SandboxRecord::owner_node` for every cell provisioned
    /// HERE, and what `ensure_cell` and the idle reap compare a record's `owner_node` against.
    node_name: String,
}

impl PlatformSandboxProvider {
    pub fn new(region: String, backend: Option<Arc<dyn CellBackend>>, node_name: String) -> Self {
        PlatformSandboxProvider {
            sandboxes: Arc::new(PLRwLock::new(Vec::new())),
            commands: Arc::new(PLRwLock::new(Vec::new())),
            snapshots: Arc::new(PLRwLock::new(Vec::new())),
            mounts: Arc::new(PLRwLock::new(Vec::new())),
            cells: Arc::new(PLRwLock::new(HashMap::new())),
            killers: Arc::new(PLRwLock::new(HashMap::new())),
            open_shells: Arc::new(PLRwLock::new(HashSet::new())),
            backend,
            region,
            node_name,
        }
    }

    /// Whether THIS node can provision a sandbox cell and execute commands in it: the same
    /// `sandbox_exec_capable` predicate the remote-candidate filter uses, applied to the locally
    /// selected backend's name, so a node never provisions locally what it would refuse to place on
    /// a peer.
    pub fn local_exec_capable(&self) -> bool {
        self.backend
            .as_ref()
            .is_some_and(|b| sandbox_exec_capable(b.name()))
    }

    /// Name of the locally selected backend ("firecracker"/"litebox"), or
    /// `None` when this node has no real isolation backend at all.
    pub fn backend_name(&self) -> Option<&'static str> {
        self.backend.as_ref().map(|b| b.name())
    }

    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    /// Mark `sandbox_id` as having a live terminal session. Paired with
    /// `end_shell_session`, which the caller's `Drop` guard runs.
    pub fn begin_shell_session(&self, sandbox_id: &str) {
        self.open_shells.write().insert(sandbox_id.to_string());
    }
    pub fn end_shell_session(&self, sandbox_id: &str) {
        self.open_shells.write().remove(sandbox_id);
    }

    /// Adopts a record provisioned on ANOTHER node (its `owner_node` names the peer) as local
    /// metadata; never provisions a cell here. The leader keeps the copy so a later
    /// leader-forwarded mutation can find the owner to route to, and adopting the same id again
    /// replaces it in place, so a retried delegation is idempotent.
    pub fn adopt_record(&self, rec: SandboxRecord) {
        let mut w = self.sandboxes.write();
        if let Some(existing) = w.iter_mut().find(|s| s.id == rec.id) {
            *existing = rec;
        } else {
            w.push(rec);
        }
    }

    /// Drop the local metadata copy of a sandbox the OWNER has already deleted
    /// (no cell is touched here — this node never held one).
    pub fn forget_record(&self, project_id: &str, id: &str) {
        let now = hive_core::now_ms();
        let mut w = self.sandboxes.write();
        if let Some(rec) = w
            .iter_mut()
            .find(|s| s.project_id == project_id && s.id == id && s.deleted_at.is_none())
        {
            rec.deleted_at = Some(now);
            rec.status = SandboxStatus::Stopped;
            rec.updated_at = now;
        }
    }

    /// Looks a sandbox up by id ALONE, with no project scope. The only caller is
    /// `mesh_shell::resolve_sandbox_shell`, which binds the record to the project in the opener's
    /// handshake and checks that this node owns it before serving anything; the id is a minted
    /// random value (`mint_sandbox_id`), never tenant input.
    pub fn get_sandbox_by_id(&self, id: &str) -> Option<SandboxRecord> {
        self.sandboxes
            .read()
            .iter()
            .find(|s| s.id == id && s.deleted_at.is_none())
            .cloned()
    }

    /// Renames a project's sandboxes IN PLACE: live records plus their command, snapshot and mount
    /// history. Sandbox ids are minted (never derived from the project name), so nothing on disk or
    /// in the cell moves, only the ownership label every list/authorize call filters on.
    pub fn rename_project(&self, from: &str, to: &str) -> usize {
        if from == to {
            return 0;
        }
        let mut n = 0;
        {
            let mut w = self.sandboxes.write();
            for rec in w.iter_mut().filter(|s| s.project_id == from) {
                rec.project_id = to.to_string();
                n += 1;
            }
        }
        {
            let mut w = self.commands.write();
            for rec in w.iter_mut().filter(|c| c.project_id == from) {
                rec.project_id = to.to_string();
                n += 1;
            }
        }
        {
            let mut w = self.snapshots.write();
            for rec in w.iter_mut().filter(|s| s.project_id == from) {
                rec.project_id = to.to_string();
                n += 1;
            }
        }
        {
            let mut w = self.mounts.write();
            for rec in w.iter_mut().filter(|m| m.project_id == from) {
                rec.project_id = to.to_string();
                n += 1;
            }
        }
        n
    }

    pub fn snapshot(
        &self,
    ) -> (
        Vec<SandboxRecord>,
        Vec<SandboxCommandRecord>,
        Vec<SandboxSnapshotRecord>,
        Vec<SandboxMountRecord>,
    ) {
        (
            self.sandboxes.read().clone(),
            self.commands.read().clone(),
            self.snapshots.read().clone(),
            self.mounts.read().clone(),
        )
    }
    /// Replaces the in-memory sets from a persisted snapshot. Cells do not survive a restart, so
    /// every record persisted as `Running` comes back `Stopped` (its next use provisions a fresh
    /// cell) with `last_stopped_at` stamped, which keeps a record stopped by a restart
    /// distinguishable from one never started.
    pub fn load(
        &self,
        s: Vec<SandboxRecord>,
        c: Vec<SandboxCommandRecord>,
        sn: Vec<SandboxSnapshotRecord>,
        m: Vec<SandboxMountRecord>,
    ) {
        *self.sandboxes.write() = s;
        *self.commands.write() = c;
        *self.snapshots.write() = sn;
        *self.mounts.write() = m;
        let now = hive_core::now_ms();
        for rec in self.sandboxes.write().iter_mut() {
            if rec.status == SandboxStatus::Running {
                rec.status = SandboxStatus::Stopped;
                rec.last_stopped_at = Some(now);
            }
        }
    }

    pub fn count_for_project(&self, project_id: &str) -> u32 {
        self.sandboxes
            .read()
            .iter()
            .filter(|s| s.project_id == project_id && s.deleted_at.is_none())
            .count() as u32
    }
    pub fn count_running_for_project(&self, project_id: &str) -> u32 {
        self.sandboxes
            .read()
            .iter()
            .filter(|s| {
                s.project_id == project_id
                    && s.deleted_at.is_none()
                    && s.status == SandboxStatus::Running
            })
            .count() as u32
    }

    /// Idle-timeout reap: a real deadline and a real teardown (the idle/drain pattern of
    /// `fluid_compute::reconcile()`), never just a status flip. A sandbox with a command in flight
    /// or an open shell is BUSY, not idle: its cell is never torn down mid-command, and its
    /// deadline restarts from now so the countdown the UI shows never runs out under live work.
    /// Only the OWNER reaps: an adopted metadata copy has no cell, and flipping it to Stopped would
    /// claim a state the owner has not reached.
    ///
    /// Returns how many sandboxes it stopped. The stopped records live only in memory until
    /// something persists, so the CALLER persists when the count is non-zero (this provider holds
    /// no `CloudState`).
    pub async fn reap_expired(&self) -> usize {
        let now = hive_core::now_ms();
        let busy: HashSet<String> = self
            .commands
            .read()
            .iter()
            .filter(|c| matches!(c.status, CommandStatus::Queued | CommandStatus::Running))
            .map(|c| c.sandbox_id.clone())
            .chain(self.open_shells.read().iter().cloned())
            .collect();
        let expired: Vec<(String, String)> = self
            .sandboxes
            .read()
            .iter()
            .filter(|s| {
                s.status == SandboxStatus::Running
                    && s.timeout_expires_at.map(|t| now >= t).unwrap_or(false)
                    && !busy.contains(&s.id)
                    && (s.owner_node.is_empty() || s.owner_node == self.node_name)
            })
            .map(|s| (s.project_id.clone(), s.id.clone()))
            .collect();
        let mut reaped = 0usize;
        let mut kept = 0usize;
        for (project_id, id) in expired {
            if busy.contains(&id) {
                self.mark_running(&project_id, &id, None);
                kept += 1;
            } else if SandboxProvider::stop_sandbox(self, &project_id, &id)
                .await
                .is_ok()
            {
                reaped += 1;
            }
        }
        if reaped > 0 || kept > 0 {
            tracing::info!(
                reaped,
                kept_busy = kept,
                node = %self.node_name,
                "sandbox idle timeout sweep"
            );
        }
        reaped
    }

    fn secret_values(&self, sandbox: &SandboxRecord) -> Vec<String> {
        sandbox
            .env_refs
            .iter()
            .map(|e| crate::secrets::decrypt(&e.value_enc))
            .filter(|v| !v.is_empty())
            .collect()
    }

    fn find_mut<'a>(
        v: &'a mut Vec<SandboxRecord>,
        project_id: &str,
        id: &str,
    ) -> Result<&'a mut SandboxRecord, SandboxError> {
        v.iter_mut()
            .find(|s| s.project_id == project_id && s.id == id && s.deleted_at.is_none())
            .ok_or_else(|| SandboxError::NotFound(id.to_string()))
    }

    /// The typed refusal every cell-touching path on a backend-less node returns; its message names
    /// which backend this node lacks.
    fn engine_unavailable(&self) -> SandboxError {
        SandboxError::EngineUnavailable(format!(
            "node {} has no exec-capable isolation backend for sandboxes (Firecracker needs Linux + /dev/kvm; Litebox needs a verified runner + HIVE_LITEBOX_VERIFIED=1); backend here: {}",
            self.node_name,
            self.backend_name().unwrap_or("none")
        ))
    }

    /// Provisions a cell for `sandbox` WITHOUT publishing it: the returned guard owns the cell
    /// until `commit` inserts it into `cells`, and dropping the guard on any error or cancel path
    /// terminates the cell.
    async fn provision_cell(
        &self,
        sandbox: &SandboxRecord,
    ) -> Result<ProvisionedCellGuard, SandboxError> {
        let backend = self
            .backend
            .clone()
            .ok_or_else(|| self.engine_unavailable())?;
        let image = sandbox
            .current_snapshot_id
            .clone()
            .unwrap_or_else(|| runtime_image(&sandbox.runtime).to_string());
        let spec = CellSpec {
            id: cell_id_for(&sandbox.id),
            image,
            resources: ResourceSpec {
                vcpus: sandbox.vcpus.max(1),
                mem_mib: sandbox.memory_mb.max(512),
                disk_mib: 2048,
                timeout_secs: (sandbox.timeout_ms / 1000).max(60),
            },
            tenant: sandbox.tenant_id.clone(),
            container: None,
        };
        let handle = backend.provision(&spec).await.map_err(|e| {
            SandboxError::EngineUnavailable(format!(
                "node {} ({}) failed to provision the sandbox cell: {e:#}",
                self.node_name,
                backend.name()
            ))
        })?;
        Ok(ProvisionedCellGuard {
            sandbox_id: sandbox.id.clone(),
            handle: Some(handle),
            backend,
            cells: self.cells.clone(),
        })
    }

    /// Provision (or re-provision, if the cell was lost to a restart) the
    /// sandbox's cell and return its handle. Idempotent per sandbox id.
    /// A record whose `owner_node` is another node is only a metadata copy here: provisioning a
    /// SECOND cell would fork the sandbox across two nodes, so that case answers
    /// `OwnerUnreachable`.
    async fn ensure_cell(&self, sandbox: &SandboxRecord) -> Result<CellHandle, SandboxError> {
        if let Some(h) = self.cells.read().get(&sandbox.id).cloned() {
            return Ok(h);
        }
        if !sandbox.owner_node.is_empty() && sandbox.owner_node != self.node_name {
            return Err(SandboxError::OwnerUnreachable(format!(
                "sandbox {} is owned by node {}; this node ({}) only holds its metadata",
                sandbox.id, sandbox.owner_node, self.node_name
            )));
        }
        let guard = self.provision_cell(sandbox).await?;
        Ok(guard.commit())
    }

    /// Owner-side create with a CALLER-MINTED id (the leader mints before any transport).
    /// Idempotent on that id: a retry after a timed-out delegation returns the record the first
    /// attempt already committed instead of provisioning twice. Fails closed: a provisioning
    /// failure returns the typed `EngineUnavailable` and persists NOTHING, and the armed
    /// `ProvisionedCellGuard` releases the cell if anything before the commit errors or is
    /// cancelled. The id/name uniqueness check is repeated under the write lock after provisioning:
    /// a concurrent create that committed meanwhile wins, and the guard tears this duplicate cell
    /// down.
    pub async fn create_sandbox_with_id(
        &self,
        tenant_id: &str,
        project_id: &str,
        id: &str,
        input: CreateSandboxInput,
    ) -> Result<SandboxRecord, SandboxError> {
        validate_name(&input.name)?;
        validate_runtime(&input.runtime)?;
        validate_network_policy(&input.network_policy)?;
        if id.is_empty() || !id.starts_with("sbx_") || id.len() > 64 {
            return Err(SandboxError::InvalidName(format!(
                "sandbox id '{id}' is not a platform-minted id"
            )));
        }
        {
            let r = self.sandboxes.read();
            if let Some(existing) = r.iter().find(|s| s.id == id && s.deleted_at.is_none()) {
                if existing.tenant_id == tenant_id && existing.project_id == project_id {
                    return Ok(existing.clone());
                }
                return Err(SandboxError::AlreadyExists(format!(
                    "sandbox id {id} already exists under a different project"
                )));
            }
            if r.iter().any(|s| {
                s.project_id == project_id && s.name == input.name && s.deleted_at.is_none()
            }) {
                return Err(SandboxError::AlreadyExists(format!(
                    "sandbox '{}' already exists in this project",
                    input.name
                )));
            }
        }
        if !self.local_exec_capable() {
            return Err(self.engine_unavailable());
        }

        let now = hive_core::now_ms();
        let env_refs: Vec<EnvRef> = input
            .env
            .into_iter()
            .map(|(k, v, _sensitive)| EnvRef {
                key: k,
                value_enc: crate::secrets::encrypt(&v),
            })
            .collect();

        let mut rec = SandboxRecord {
            id: id.to_string(),
            tenant_id: tenant_id.to_string(),
            project_id: project_id.to_string(),
            provider: "platform".into(),
            provider_sandbox_name: id.to_string(),
            provider_sandbox_id: None,
            name: input.name,
            status: SandboxStatus::Pending,
            runtime: input.runtime,
            region: self.region.clone(),
            vcpus: input.vcpus.max(1),
            memory_mb: if input.memory_mb > 0 {
                input.memory_mb
            } else {
                1024
            },
            timeout_ms: if input.timeout_ms > 0 {
                input.timeout_ms
            } else {
                5 * 60_000
            },
            timeout_expires_at: None,
            persistent: input.persistent,
            ports: input.ports,
            exposed_domains: HashMap::new(),
            network_policy: input.network_policy,
            env_refs,
            tags: input.tags,
            current_snapshot_id: None,
            snapshot_expiration_ms: None,
            keep_last_snapshots: None,
            active_cpu_usage_ms: 0,
            total_duration_ms: 0,
            total_active_cpu_duration_ms: 0,
            total_ingress_bytes: 0,
            total_egress_bytes: 0,
            last_started_at: None,
            last_stopped_at: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            container: String::new(),
            note: String::new(),
            owner_node: self.node_name.clone(),
        };
        rec.timeout_expires_at = Some(now + rec.timeout_ms);

        let guard = self.provision_cell(&rec).await?;
        {
            let mut w = self.sandboxes.write();
            if let Some(existing) = w.iter().find(|s| s.id == rec.id && s.deleted_at.is_none()) {
                return Ok(existing.clone());
            }
            if w.iter().any(|s| {
                s.project_id == rec.project_id && s.name == rec.name && s.deleted_at.is_none()
            }) {
                return Err(SandboxError::AlreadyExists(format!(
                    "sandbox '{}' already exists in this project",
                    rec.name
                )));
            }
            let handle = guard.commit();
            rec.status = SandboxStatus::Running;
            rec.provider_sandbox_id = handle.endpoint.clone();
            rec.container = cell_id_for(&rec.id).to_string();
            rec.last_started_at = Some(now);
            w.push(rec.clone());
        }
        tracing::info!(sandbox = %rec.id, node = %self.node_name, backend = self.backend_name().unwrap_or("none"), "sandbox cell provisioned");
        Ok(rec)
    }

    /// Publishes that this sandbox HAS a live cell right now: status `Running` plus a fresh idle
    /// deadline. The ONE place a record becomes `Running` after create, so every path that
    /// (re)provisions a cell (`start`, or an exec/shell that resurrected one through `ensure_cell`)
    /// reports the same state instead of leaving a stopped record over a live cell.
    ///
    /// Idempotent for accounting: an already-`Running` record keeps its `last_started_at`, so
    /// `stop`'s `last_stopped_at - last_started_at` accumulation never double-counts.
    fn mark_running(&self, project_id: &str, id: &str, endpoint: Option<String>) {
        let mut w = self.sandboxes.write();
        let Some(rec) = w
            .iter_mut()
            .find(|s| s.project_id == project_id && s.id == id && s.deleted_at.is_none())
        else {
            return;
        };
        let now = hive_core::now_ms();
        if rec.status != SandboxStatus::Running {
            rec.status = SandboxStatus::Running;
            rec.last_started_at = Some(now);
            rec.last_stopped_at = None;
        }
        rec.timeout_expires_at = Some(now + rec.timeout_ms);
        rec.updated_at = now;
        rec.container = cell_id_for(&rec.id).to_string();
        if let Some(ep) = endpoint {
            rec.provider_sandbox_id = Some(ep);
        }
    }

    /// Tears the sandbox's cell down and forgets it. The handle is dropped even when teardown fails
    /// (the caller's state change is what was asked for), but the failure is logged at WARN naming
    /// the orphan: `reap_orphaned_runners` runs only at litebox backend construction, so a leaked
    /// runner keeps its TUN device and cell root dir until the next boot.
    async fn terminate_cell(&self, sandbox_id: &str) {
        let handle = self.cells.write().remove(sandbox_id);
        if let (Some(handle), Some(backend)) = (handle, self.backend.as_ref()) {
            if let Err(e) = backend.terminate(&handle).await {
                tracing::warn!(
                    sandbox = %sandbox_id,
                    node = %self.node_name,
                    error = %e,
                    "sandbox cell teardown FAILED — the provider has forgotten this cell but its runner, TUN device and root dir may still exist"
                );
            }
        }
    }
}

#[async_trait::async_trait]
impl SandboxProvider for PlatformSandboxProvider {
    async fn list_sandboxes(&self, project_id: &str) -> Result<Vec<SandboxRecord>, SandboxError> {
        Ok(self
            .sandboxes
            .read()
            .iter()
            .filter(|s| s.project_id == project_id && s.deleted_at.is_none())
            .cloned()
            .collect())
    }

    /// Local create with a freshly minted id — see `create_sandbox_with_id`
    /// for the fail-closed contract (this is the same path, id minted here).
    async fn create_sandbox(
        &self,
        tenant_id: &str,
        project_id: &str,
        input: CreateSandboxInput,
    ) -> Result<SandboxRecord, SandboxError> {
        let id = mint_sandbox_id();
        self.create_sandbox_with_id(tenant_id, project_id, &id, input)
            .await
    }

    async fn get_sandbox(
        &self,
        project_id: &str,
        id_or_name: &str,
    ) -> Result<SandboxRecord, SandboxError> {
        self.sandboxes
            .read()
            .iter()
            .find(|s| {
                s.project_id == project_id
                    && s.deleted_at.is_none()
                    && (s.id == id_or_name || s.name == id_or_name)
            })
            .cloned()
            .ok_or_else(|| SandboxError::NotFound(id_or_name.to_string()))
    }

    /// Returns the project's sandbox named `input.name`, creating it when absent. A `Failed` row
    /// can only be a record persisted before create failed closed (this provider no longer writes
    /// one); returning it would present a sandbox that never had a cell as existing, so it surfaces
    /// as the typed failure instead.
    async fn get_or_create_sandbox(
        &self,
        tenant_id: &str,
        project_id: &str,
        input: CreateSandboxInput,
    ) -> Result<SandboxRecord, SandboxError> {
        if let Ok(existing) = self.get_sandbox(project_id, &input.name).await {
            if existing.status == SandboxStatus::Failed {
                return Err(SandboxError::EngineUnavailable(format!(
                    "sandbox '{}' ({}) has no cell — it was recorded as failed ({}); delete it and create again",
                    existing.name,
                    existing.id,
                    if existing.note.is_empty() {
                        "no detail recorded"
                    } else {
                        existing.note.as_str()
                    }
                )));
            }
            return Ok(existing);
        }
        self.create_sandbox(tenant_id, project_id, input).await
    }

    /// Starts a sandbox on this node. A cell lost to `stop`, `delete` or a restart is gone for
    /// good, so a start is always a fresh provision through `ensure_cell` (the one
    /// provision/resurrect primitive: it returns the live handle when this node still holds the
    /// cell), never a resume of dead state.
    async fn start_sandbox(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<SandboxRecord, SandboxError> {
        let sandbox = self.get_sandbox(project_id, id).await?;
        if !self.local_exec_capable() {
            return Err(self.engine_unavailable());
        }
        let handle = self.ensure_cell(&sandbox).await?;
        self.mark_running(project_id, &sandbox.id, handle.endpoint.clone());
        tracing::info!(sandbox = %sandbox.id, node = %self.node_name, backend = self.backend_name().unwrap_or("none"), "sandbox cell started");
        self.get_sandbox(project_id, &sandbox.id).await
    }

    /// Stops a sandbox: a persistent one with a live cell first takes a record-level snapshot
    /// (`create_snapshot`), then the cell is terminated and the run is added to
    /// `total_duration_ms`.
    async fn stop_sandbox(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<SandboxRecord, SandboxError> {
        let sandbox = self.get_sandbox(project_id, id).await?;
        if sandbox.persistent && self.cells.read().contains_key(id) {
            let _ = SandboxProvider::create_snapshot(
                self,
                project_id,
                id,
                CreateSnapshotInput::default(),
            )
            .await;
        }
        self.terminate_cell(id).await;
        let mut w = self.sandboxes.write();
        let rec = Self::find_mut(&mut w, project_id, id)?;
        rec.status = SandboxStatus::Stopped;
        rec.last_stopped_at = Some(hive_core::now_ms());
        rec.updated_at = hive_core::now_ms();
        if let Some(started) = rec.last_started_at {
            rec.total_duration_ms += rec
                .last_stopped_at
                .unwrap_or(started)
                .saturating_sub(started);
        }
        Ok(rec.clone())
    }

    async fn delete_sandbox(&self, project_id: &str, id: &str) -> Result<(), SandboxError> {
        self.terminate_cell(id).await;
        let mut w = self.sandboxes.write();
        let rec = Self::find_mut(&mut w, project_id, id)?;
        rec.deleted_at = Some(hive_core::now_ms());
        rec.status = SandboxStatus::Stopped;
        Ok(())
    }

    /// Runs a command in the sandbox's cell, provisioning the cell first when it was lost. Every
    /// exec refreshes the idle deadline through `mark_running` (idempotent), so the timeout is
    /// measured from the last use of the sandbox, never from its creation; applied only while
    /// `status != Running` it would let a busy sandbox be reaped at its original deadline
    /// mid-command. The drain and its deadline run in a supervisor task (`supervise_exec`) for both
    /// modes, and a blocking call only awaits it.
    async fn run_command(
        &self,
        project_id: &str,
        id: &str,
        input: RunCommandInput,
    ) -> Result<SandboxCommandRecord, SandboxError> {
        validate_argv(&input.cmd, &input.args)?;
        let sandbox = self.get_sandbox(project_id, id).await?;
        let backend = self
            .backend
            .as_ref()
            .ok_or_else(|| self.engine_unavailable())?;
        let handle = self.ensure_cell(&sandbox).await?;
        self.mark_running(project_id, &sandbox.id, handle.endpoint.clone());
        let secrets = self.secret_values(&sandbox);
        let cmd_id = format!("cmd_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        let now = hive_core::now_ms();

        let mut env = sandbox
            .env_refs
            .iter()
            .map(|e| (e.key.clone(), crate::secrets::decrypt(&e.value_enc)))
            .collect::<HashMap<_, _>>();
        for (k, v) in &input.env {
            env.insert(k.clone(), v.clone());
        }
        let guest_req = GuestExecRequest {
            id: cmd_id.clone(),
            cmd: input.cmd.clone(),
            args: input.args.clone(),
            cwd: input.cwd.clone(),
            env: env.into_iter().collect(),
            sudo: input.sudo,
            shell: input.shell,
        };

        let base_rec = SandboxCommandRecord {
            id: cmd_id.clone(),
            tenant_id: sandbox.tenant_id.clone(),
            project_id: project_id.to_string(),
            sandbox_id: id.to_string(),
            provider_command_id: None,
            cmd: input.cmd,
            args: input.args,
            cwd: input.cwd,
            env_refs: vec![],
            sudo: input.sudo,
            detached: input.detached,
            status: CommandStatus::Running,
            exit_code: None,
            stdout: vec![],
            stderr: vec![],
            started_at: Some(now),
            finished_at: None,
            created_at: now,
            updated_at: now,
        };
        self.commands.write().push(base_rec.clone());

        let rx = backend
            .exec_command(&handle, guest_req)
            .await
            .map_err(|e| SandboxError::EngineUnavailable(format!("exec failed to start: {e}")))?;

        let notify = Arc::new(tokio::sync::Notify::new());
        self.killers.write().insert(cmd_id.clone(), notify.clone());

        let deadline = if input.detached {
            detached_exec_ceiling(sandbox.timeout_ms)
        } else {
            blocking_run_deadline()
        };
        let supervisor = tokio::spawn(supervise_exec(
            rx,
            self.commands.clone(),
            self.killers.clone(),
            cmd_id.clone(),
            secrets,
            notify,
            backend.clone(),
            handle.clone(),
            deadline,
        ));
        if input.detached {
            return Ok(base_rec);
        }
        let _ = supervisor.await;
        self.get_command(project_id, id, &cmd_id).await
    }

    /// Opens an interactive pty in the sandbox's cell. Like `run_command` it marks the record
    /// running again and refreshes the idle deadline, so an open shell is never reaped from under
    /// the user. The runtime's interpreter is requested through `ExecPtyRequest::programs`
    /// (`shell_runtime_programs`): the litebox guest tar holds only sh and coreutils unless told
    /// otherwise, so without it `node` is not found in a node sandbox's shell; Firecracker ignores
    /// the list (its rootfs has the runtime).
    async fn open_shell(
        &self,
        project_id: &str,
        id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<
        (
            tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>,
            hive_backend::PtyIo,
        ),
        SandboxError,
    > {
        let sandbox = self.get_sandbox(project_id, id).await?;
        let backend = self
            .backend
            .as_ref()
            .ok_or_else(|| self.engine_unavailable())?;
        let handle = self.ensure_cell(&sandbox).await?;
        self.mark_running(project_id, &sandbox.id, handle.endpoint.clone());
        let session_id = format!("sh_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);

        let env = sandbox
            .env_refs
            .iter()
            .map(|e| (e.key.clone(), crate::secrets::decrypt(&e.value_enc)))
            .collect::<std::collections::BTreeMap<_, _>>();

        let programs = shell_runtime_programs(&sandbox.runtime);
        let req = hive_core::ExecPtyRequest {
            id: session_id,
            shell: String::new(),
            cwd: String::new(),
            env,
            cols,
            rows,
            programs,
        };
        backend
            .exec_pty(&handle, req)
            .await
            .map_err(|e| SandboxError::EngineUnavailable(format!("pty failed to start: {e}")))
    }

    async fn list_commands(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Vec<SandboxCommandRecord>, SandboxError> {
        Ok(self
            .commands
            .read()
            .iter()
            .filter(|c| c.project_id == project_id && c.sandbox_id == id)
            .cloned()
            .collect())
    }

    async fn get_command(
        &self,
        project_id: &str,
        id: &str,
        command_id: &str,
    ) -> Result<SandboxCommandRecord, SandboxError> {
        self.commands
            .read()
            .iter()
            .find(|c| c.project_id == project_id && c.sandbox_id == id && c.id == command_id)
            .cloned()
            .ok_or_else(|| SandboxError::CommandNotFound(command_id.to_string()))
    }

    /// Kills a command: the project-scoped sandbox lookup is the authorization, the guest is told
    /// through `kill_exec` and the drain's `Notify` fires. After a 300 ms grace for the drain to
    /// observe `ExecDone`, a record still `Running` is marked `Killed` here, so the caller never
    /// reads a stale `Running` status.
    async fn kill_command(
        &self,
        project_id: &str,
        id: &str,
        command_id: &str,
    ) -> Result<SandboxCommandRecord, SandboxError> {
        let sandbox = self.get_sandbox(project_id, id).await?;
        let _ = sandbox;
        let handle = self.cells.read().get(id).cloned();
        if let Some(handle) = handle {
            if let Some(backend) = self.backend.as_ref() {
                let _ = backend.kill_exec(&handle, command_id).await;
            }
        }
        if let Some(notify) = self.killers.read().get(command_id) {
            notify.notify_waiters();
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut w = self.commands.write();
        let rec = w
            .iter_mut()
            .find(|c| c.project_id == project_id && c.sandbox_id == id && c.id == command_id)
            .ok_or_else(|| SandboxError::CommandNotFound(command_id.to_string()))?;
        if matches!(rec.status, CommandStatus::Running) {
            rec.status = CommandStatus::Killed;
            rec.finished_at = Some(hive_core::now_ms());
            rec.updated_at = rec.finished_at.unwrap();
        }
        Ok(rec.clone())
    }

    async fn write_files(
        &self,
        project_id: &str,
        id: &str,
        files: Vec<(String, Vec<u8>)>,
    ) -> Result<(), SandboxError> {
        for (path, bytes) in files {
            if !path.starts_with('/') || path.contains("..") {
                return Err(SandboxError::InvalidMount(format!(
                    "path '{path}' must be absolute with no '..' segments"
                )));
            }
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let dir = std::path::Path::new(&path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if !dir.is_empty() {
                self.run_command(
                    project_id,
                    id,
                    RunCommandInput {
                        cmd: "mkdir".into(),
                        args: vec!["-p".into(), dir],
                        ..Default::default()
                    },
                )
                .await?;
            }
            let write_cmd = format!("echo '{b64}' | base64 -d > '{path}'");
            let res = self
                .run_command(
                    project_id,
                    id,
                    RunCommandInput {
                        cmd: "sh".into(),
                        args: vec!["-c".into(), write_cmd],
                        shell: false,
                        ..Default::default()
                    },
                )
                .await?;
            if res.exit_code != Some(0) {
                return Err(SandboxError::EngineUnavailable(format!(
                    "failed to write {path} (exit {:?})",
                    res.exit_code
                )));
            }
        }
        Ok(())
    }

    async fn read_file(
        &self,
        project_id: &str,
        id: &str,
        path: &str,
    ) -> Result<Vec<u8>, SandboxError> {
        if !path.starts_with('/') || path.contains("..") {
            return Err(SandboxError::InvalidMount(format!(
                "path '{path}' must be absolute with no '..' segments"
            )));
        }
        let res = self
            .run_command(
                project_id,
                id,
                RunCommandInput {
                    cmd: "base64".into(),
                    args: vec![path.to_string()],
                    ..Default::default()
                },
            )
            .await?;
        if res.exit_code != Some(0) {
            return Err(SandboxError::NotFound(format!(
                "file not found in sandbox: {path}"
            )));
        }
        let text: String = res
            .stdout
            .iter()
            .map(|l| l.line.clone())
            .collect::<Vec<_>>()
            .join("");
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .map_err(|e| {
                SandboxError::EngineUnavailable(format!("could not decode file contents: {e}"))
            })
    }

    /// Records a lightweight snapshot marker (the runtime's base image, `provider_snapshot_id`) for
    /// a sandbox with a live cell and makes it the sandbox's `current_snapshot_id`; snapshots
    /// beyond `keep_last` are deleted. No live memory or disk state is captured: Firecracker's
    /// `/snapshot/create` (pause + dump) is not wired up.
    async fn create_snapshot(
        &self,
        project_id: &str,
        id: &str,
        input: CreateSnapshotInput,
    ) -> Result<SandboxSnapshotRecord, SandboxError> {
        let sandbox = self.get_sandbox(project_id, id).await?;
        if !self.cells.read().contains_key(id) {
            return Err(SandboxError::EngineUnavailable(
                "sandbox has no live microVM to snapshot".into(),
            ));
        }
        let snap_id = format!("snap_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        let now = hive_core::now_ms();
        let rec = SandboxSnapshotRecord {
            id: snap_id,
            tenant_id: sandbox.tenant_id.clone(),
            project_id: project_id.to_string(),
            sandbox_id: id.to_string(),
            provider_snapshot_id: runtime_image(&sandbox.runtime).to_string(),
            source_session_id: Some(sandbox.id.clone()),
            status: "ready".into(),
            size_bytes: None,
            created_at: now,
            expires_at: sandbox.snapshot_expiration_ms.map(|ms| now + ms),
        };
        self.snapshots.write().push(rec.clone());
        {
            let mut w = self.sandboxes.write();
            if let Ok(s) = Self::find_mut(&mut w, project_id, id) {
                s.current_snapshot_id = Some(rec.id.clone());
                s.updated_at = now;
            }
        }
        if let Some(keep) = input.keep_last.or(sandbox.keep_last_snapshots) {
            let stale: Vec<SandboxSnapshotRecord> = {
                let mut mine: Vec<SandboxSnapshotRecord> = self
                    .snapshots
                    .read()
                    .iter()
                    .filter(|s| s.sandbox_id == id)
                    .cloned()
                    .collect();
                mine.sort_by_key(|s| std::cmp::Reverse(s.created_at));
                mine.split_off(keep.max(1) as usize)
            };
            for s in stale {
                let _ = SandboxProvider::delete_snapshot(self, project_id, &s.id).await;
            }
        }
        Ok(rec)
    }

    async fn list_snapshots(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Vec<SandboxSnapshotRecord>, SandboxError> {
        Ok(self
            .snapshots
            .read()
            .iter()
            .filter(|s| s.project_id == project_id && s.sandbox_id == id)
            .cloned()
            .collect())
    }

    async fn delete_snapshot(
        &self,
        project_id: &str,
        snapshot_id: &str,
    ) -> Result<(), SandboxError> {
        let mut w = self.snapshots.write();
        let before = w.len();
        w.retain(|s| !(s.project_id == project_id && s.id == snapshot_id));
        if w.len() == before {
            return Err(SandboxError::SnapshotNotFound(snapshot_id.to_string()));
        }
        Ok(())
    }

    /// The intended preview URL for an exposed port, `http://<HIVE_PUBLIC_IP or 127.0.0.1>:<port>`.
    /// It is not routable yet: a public preview needs a host-side port-forward or edge route, and a
    /// cell's networking is per-cell (`setup_cell_net`: a TAP on Firecracker, a TUN on Litebox), so
    /// this only fixes the URL contract for the UI and API.
    async fn domain(&self, project_id: &str, id: &str, port: u16) -> Result<String, SandboxError> {
        let sandbox = self.get_sandbox(project_id, id).await?;
        if !sandbox.ports.contains(&port) {
            return Err(SandboxError::InvalidMount(format!(
                "port {port} was not exposed at sandbox creation — add it to `ports` first"
            )));
        }
        let host = std::env::var("HIVE_PUBLIC_IP")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        Ok(format!("http://{host}:{port}"))
    }

    /// Stores the new policy on the record. A running cell's network namespace is fixed at boot
    /// (Firecracker has no live network-policy toggle), so the change takes effect on the sandbox's
    /// NEXT provision, not instantaneously.
    async fn update_network_policy(
        &self,
        project_id: &str,
        id: &str,
        policy: NetworkPolicy,
    ) -> Result<SandboxRecord, SandboxError> {
        validate_network_policy(&policy)?;
        let mut w = self.sandboxes.write();
        let rec = Self::find_mut(&mut w, project_id, id)?;
        rec.network_policy = policy;
        rec.updated_at = hive_core::now_ms();
        Ok(rec.clone())
    }

    /// Validates and records a storage mount (credentials sealed with `secrets::encrypt`).
    /// Attaching it to the microVM is not wired: persistent drives and remote-FUSE mounts need a
    /// second attached drive or an in-guest FUSE driver, which the boot path lacks (its data-disk
    /// mechanism serves delivered builds only), so the record reports status `pending` instead of a
    /// fake `mounted`.
    async fn mount_storage(
        &self,
        project_id: &str,
        id: &str,
        input: MountConfigInput,
    ) -> Result<SandboxMountRecord, SandboxError> {
        validate_mount(&input.mount_path, &input.kind, &input.mode, &input.provider)?;
        let sandbox = self.get_sandbox(project_id, id).await?;
        let now = hive_core::now_ms();
        let mount_id = format!("mnt_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);

        let mut config_refs = HashMap::new();
        for (k, v) in [
            ("bucket", &input.bucket),
            ("region", &input.region),
            ("endpoint", &input.endpoint),
        ] {
            if !v.is_empty() {
                config_refs.insert(k.to_string(), v.clone());
            }
        }
        if !input.access_key.is_empty() {
            config_refs.insert(
                "access_key_enc".into(),
                crate::secrets::encrypt(&input.access_key),
            );
        }
        if !input.secret_key.is_empty() {
            config_refs.insert(
                "secret_key_enc".into(),
                crate::secrets::encrypt(&input.secret_key),
            );
        }
        for (k, v) in &input.extra_args {
            config_refs.insert(format!("extra_{k}"), v.clone());
        }

        let (status, note) = ("pending", "mount is configured; attaching it to the sandbox's microVM is not yet wired up on this node".to_string());
        let _ = &sandbox;

        let rec = SandboxMountRecord {
            id: mount_id,
            tenant_id: sandbox.tenant_id.clone(),
            project_id: project_id.to_string(),
            sandbox_id: id.to_string(),
            mount_path: input.mount_path,
            kind: input.kind,
            mode: input.mode,
            provider: input.provider,
            config_refs,
            status: status.to_string(),
            note,
            created_at: now,
            updated_at: now,
        };
        self.mounts.write().push(rec.clone());
        Ok(rec)
    }

    async fn list_mounts(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Vec<SandboxMountRecord>, SandboxError> {
        Ok(self
            .mounts
            .read()
            .iter()
            .filter(|m| m.project_id == project_id && m.sandbox_id == id)
            .cloned()
            .collect())
    }

    async fn delete_mount(&self, project_id: &str, mount_id: &str) -> Result<(), SandboxError> {
        let mut w = self.mounts.write();
        let before = w.len();
        w.retain(|m| !(m.project_id == project_id && m.id == mount_id));
        if w.len() == before {
            return Err(SandboxError::MountNotFound(mount_id.to_string()));
        }
        Ok(())
    }
}

/// Wall clock a BLOCKING `run_command` may hold the guest: the leader→owner forward's own budget
/// (`HIVE_SANDBOX_RUN_MS`, default 110 s, read by `sandboxes_api`) minus 10 s of headroom, so the
/// owner has killed the guest and finalized the record BEFORE the forward gives up on it: one knob,
/// two consistent bounds.
fn blocking_run_deadline() -> std::time::Duration {
    let forward_ms = env_u64("HIVE_SANDBOX_RUN_MS", 110_000);
    std::time::Duration::from_millis(forward_ms.saturating_sub(10_000).max(1_000))
}

/// Ceiling for a DETACHED exec: the sandbox's own timeout (its reaper tears the cell down
/// regardless) capped by `HIVE_SANDBOX_EXEC_MAX_MS` (default one hour), so a guest that ignores its
/// stdio, such as a spinning fork child or a runaway loop, can never outlive that.
fn detached_exec_ceiling(sandbox_timeout_ms: u64) -> std::time::Duration {
    let cap = env_u64("HIVE_SANDBOX_EXEC_MAX_MS", 3_600_000).max(1_000);
    let ms = if sandbox_timeout_ms > 0 {
        sandbox_timeout_ms.min(cap)
    } else {
        cap
    };
    std::time::Duration::from_millis(ms)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Grace between the deadline kill and closing the record by hand: the backend's waiter reports the
/// signal death as `ExecDone { None }` within milliseconds, so this only runs out when the runner
/// is unkillable.
const EXEC_KILL_GRACE_MS: u64 = 3_000;

/// Drains one exec to completion under a hard deadline, then finalizes its record, in a task of its
/// own for BOTH blocking and detached modes: a blocking caller's request future is dropped by axum
/// the moment the client gives up (a timeout, a closed tab, the leader's forward budget), and a
/// drain awaited inline would die with it, leaving the runner running unwatched and the record
/// `running` forever. The deadline lives in the same task for the same reason, so a hung guest is
/// killed whether or not anyone is still waiting for it.
///
/// On expiry the guest's whole process group is killed through the backend (`kill_exec` = `killpg`
/// on litebox, the agent's `KillExec` on firecracker), the drain gets `EXEC_KILL_GRACE_MS` to
/// observe the resulting `ExecDone`, and a record still `running` after that is closed as `killed`
/// with the reason appended to its stderr, so the tenant reads WHY in the logs they were already
/// polling.
#[allow(clippy::too_many_arguments)]
async fn supervise_exec(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>,
    commands: Arc<PLRwLock<Vec<SandboxCommandRecord>>>,
    killers: Arc<PLRwLock<HashMap<String, Arc<tokio::sync::Notify>>>>,
    cmd_id: String,
    secrets: Vec<String>,
    notify: Arc<tokio::sync::Notify>,
    backend: Arc<dyn CellBackend>,
    handle: CellHandle,
    deadline: std::time::Duration,
) {
    let started = std::time::Instant::now();
    let drained = tokio::time::timeout(
        deadline,
        drain_exec_events(&mut rx, &commands, &cmd_id, &secrets, notify.clone()),
    )
    .await;
    if drained.is_err() {
        tracing::warn!(
            command = %cmd_id,
            cell = %handle.id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            deadline_ms = deadline.as_millis() as u64,
            "sandbox exec exceeded its deadline; killing the guest process group"
        );
        if let Err(e) = backend.kill_exec(&handle, &cmd_id).await {
            tracing::warn!(command = %cmd_id, error = %e, "sandbox exec deadline kill failed");
        }
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(EXEC_KILL_GRACE_MS),
            drain_exec_events(&mut rx, &commands, &cmd_id, &secrets, notify),
        )
        .await;
        let now = hive_core::now_ms();
        let mut w = commands.write();
        if let Some(rec) = w.iter_mut().find(|c| c.id == cmd_id) {
            rec.stderr.push(LogLine {
                ts_ms: now,
                line: format!(
                    "[hive] command exceeded its {} ms deadline and its process group was killed",
                    deadline.as_millis()
                ),
            });
            if matches!(rec.status, CommandStatus::Running) {
                rec.status = CommandStatus::Killed;
                rec.exit_code = None;
                rec.finished_at = Some(now);
            }
            rec.updated_at = now;
        }
    }
    killers.write().remove(&cmd_id);
}

/// Drains one exec's event channel into its `SandboxCommandRecord`, redacting secrets and bounding
/// output (`MAX_OUTPUT_BYTES` in total, `MAX_LINE_BYTES` per line) until `ExecDone` or the kill
/// `Notify` fires (the killer already told the guest through `kill_exec`; this only stops waiting
/// on a connection whose far end may be gone).
async fn drain_exec_events(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>,
    commands: &Arc<PLRwLock<Vec<SandboxCommandRecord>>>,
    cmd_id: &str,
    secrets: &[String],
    notify: Arc<tokio::sync::Notify>,
) {
    use hive_core::{AgentEvent, LogStream};
    let mut total = 0usize;
    loop {
        let ev = tokio::select! {
            ev = rx.recv() => ev,
            _ = notify.notified() => break,
        };
        let Some(ev) = ev else { break };
        match ev {
            AgentEvent::ExecOutput { id, stream, line } if id == cmd_id => {
                if total >= MAX_OUTPUT_BYTES {
                    continue;
                }
                let redacted = redact_secrets(&bound_line(&line, MAX_LINE_BYTES), secrets);
                total += redacted.len();
                let mut w = commands.write();
                if let Some(rec) = w.iter_mut().find(|c| c.id == cmd_id) {
                    let target = if stream == LogStream::Stderr {
                        &mut rec.stderr
                    } else {
                        &mut rec.stdout
                    };
                    target.push(LogLine {
                        ts_ms: hive_core::now_ms(),
                        line: redacted,
                    });
                    rec.updated_at = hive_core::now_ms();
                }
            }
            AgentEvent::ExecDone { id, exit_code } if id == cmd_id => {
                let mut w = commands.write();
                if let Some(rec) = w.iter_mut().find(|c| c.id == cmd_id) {
                    rec.exit_code = exit_code;
                    rec.status = match exit_code {
                        Some(0) => CommandStatus::Exited,
                        Some(_) => CommandStatus::Failed,
                        None => CommandStatus::Killed,
                    };
                    rec.finished_at = Some(hive_core::now_ms());
                    rec.updated_at = rec.finished_at.unwrap();
                }
                break;
            }
            _ => {}
        }
    }
}
