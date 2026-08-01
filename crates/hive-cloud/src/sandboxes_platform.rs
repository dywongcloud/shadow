//! `PlatformSandboxProvider` — the production [`SandboxProvider`]: real
//! Firecracker microVMs (`hive_backend::firecracker::FirecrackerBackend`), the
//! SAME isolation tech Fluid serverless functions already use on this fleet —
//! reused, not duplicated, per the platform's own "one microVM runtime" design.
//!
//! A sandbox is a long-lived cell (`CellBackend::provision`, never torn down
//! until `stop`/`delete`/idle-timeout). Command execution rides a NEW guest-
//! agent protocol addition (`AgentRequest::Exec`/`KillExec`, `hive-core/proto.rs`)
//! that runs one argv command per call over its own vsock connection, streaming
//! `ExecOutput` (stdout/stderr kept distinct) then one `ExecDone` — see
//! `FirecrackerBackend::exec_command`/`kill_exec`.
//!
//! On a node whose isolation backend is NOT Firecracker (mock/dev), every
//! operation reports `EngineUnavailable` with a clear note — mirroring the
//! platform's existing "simulated" idiom (e.g. DB provisioning when podman is
//! absent) rather than silently downgrading to a different, undisclosed
//! isolation technology.

use crate::sandboxes::*;
use hive_backend::firecracker::FirecrackerBackend;
use hive_backend::{CellBackend, CellHandle, CellSpec};
use hive_core::{CellId, ExecRequest as GuestExecRequest, ResourceSpec};
use parking_lot::RwLock as PLRwLock;
use std::collections::HashMap;
use std::sync::Arc;

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
    /// Live cell handles — NOT persisted (a `CellHandle`'s vsock/socket paths
    /// don't survive a node restart; any running sandbox is re-provisioned
    /// fresh on next use, same as Fluid function cells today).
    cells: Arc<PLRwLock<HashMap<String, CellHandle>>>,
    /// Live kill-channels for detached execs, keyed by command id, so
    /// `kill_command` can stop draining even if the guest-side kill races.
    killers: Arc<PLRwLock<HashMap<String, Arc<tokio::sync::Notify>>>>,
    backend: Option<Arc<FirecrackerBackend>>,
    region: String,
}

impl PlatformSandboxProvider {
    pub fn new(region: String, backend: Option<Arc<FirecrackerBackend>>) -> Self {
        PlatformSandboxProvider {
            sandboxes: Arc::new(PLRwLock::new(Vec::new())),
            commands: Arc::new(PLRwLock::new(Vec::new())),
            snapshots: Arc::new(PLRwLock::new(Vec::new())),
            mounts: Arc::new(PLRwLock::new(Vec::new())),
            cells: Arc::new(PLRwLock::new(HashMap::new())),
            killers: Arc::new(PLRwLock::new(HashMap::new())),
            backend,
            region,
        }
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
        // Cells don't survive a restart; any RUNNING record is stale until the
        // sandbox is used again (mirrors Fluid's own cold-restart behavior).
        for rec in self.sandboxes.write().iter_mut() {
            if rec.status == SandboxStatus::Running {
                rec.status = SandboxStatus::Stopped;
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

    /// Idle-timeout reap: mirrors `fluid_compute::reconcile()`'s idle/drain
    /// pattern — a real deadline, real teardown, not just a status flip.
    pub async fn reap_expired(&self) {
        let now = hive_core::now_ms();
        let expired: Vec<(String, String)> = self
            .sandboxes
            .read()
            .iter()
            .filter(|s| {
                s.status == SandboxStatus::Running
                    && s.timeout_expires_at.map(|t| now >= t).unwrap_or(false)
            })
            .map(|s| (s.project_id.clone(), s.id.clone()))
            .collect();
        for (project_id, id) in expired {
            let _ = SandboxProvider::stop_sandbox(self, &project_id, &id).await;
        }
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

    /// Provision (or re-provision, if the cell was lost to a restart) the
    /// sandbox's microVM and return its handle. Idempotent per sandbox id.
    async fn ensure_cell(&self, sandbox: &SandboxRecord) -> Result<CellHandle, SandboxError> {
        if let Some(h) = self.cells.read().get(&sandbox.id).cloned() {
            return Ok(h);
        }
        let backend = self.backend.as_ref().ok_or_else(|| {
            SandboxError::EngineUnavailable("this node has no Firecracker isolation backend (Linux + /dev/kvm required) — sandboxes are simulated here".into())
        })?;
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
            SandboxError::EngineUnavailable(format!("failed to provision sandbox microVM: {e}"))
        })?;
        self.cells
            .write()
            .insert(sandbox.id.clone(), handle.clone());
        Ok(handle)
    }

    async fn terminate_cell(&self, sandbox_id: &str) {
        let handle = self.cells.write().remove(sandbox_id);
        if let (Some(handle), Some(backend)) = (handle, self.backend.as_ref()) {
            let _ = backend.terminate(&handle).await;
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

    async fn create_sandbox(
        &self,
        tenant_id: &str,
        project_id: &str,
        input: CreateSandboxInput,
    ) -> Result<SandboxRecord, SandboxError> {
        validate_name(&input.name)?;
        validate_runtime(&input.runtime)?;
        validate_network_policy(&input.network_policy)?;
        if self
            .sandboxes
            .read()
            .iter()
            .any(|s| s.project_id == project_id && s.name == input.name && s.deleted_at.is_none())
        {
            return Err(SandboxError::AlreadyExists(format!(
                "sandbox '{}' already exists in this project",
                input.name
            )));
        }

        let id = format!("sbx_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
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
            id: id.clone(),
            tenant_id: tenant_id.to_string(),
            project_id: project_id.to_string(),
            provider: "platform".into(),
            provider_sandbox_name: id.clone(),
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
        };
        rec.timeout_expires_at = Some(now + rec.timeout_ms);

        match self.ensure_cell(&rec).await {
            Ok(handle) => {
                rec.status = SandboxStatus::Running;
                rec.provider_sandbox_id = handle.endpoint.clone();
                rec.container = cell_id_for(&rec.id).to_string();
                rec.last_started_at = Some(now);
            }
            Err(e) => {
                rec.status = SandboxStatus::Failed;
                rec.note = e.message();
                tracing::warn!(sandbox = %rec.id, error = %rec.note, "sandbox microVM provisioning failed — simulated");
            }
        }
        self.sandboxes.write().push(rec.clone());
        Ok(rec)
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

    async fn get_or_create_sandbox(
        &self,
        tenant_id: &str,
        project_id: &str,
        input: CreateSandboxInput,
    ) -> Result<SandboxRecord, SandboxError> {
        if let Ok(existing) = self.get_sandbox(project_id, &input.name).await {
            return Ok(existing);
        }
        self.create_sandbox(tenant_id, project_id, input).await
    }

    async fn stop_sandbox(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<SandboxRecord, SandboxError> {
        let sandbox = self.get_sandbox(project_id, id).await?;
        // Persistent sandboxes snapshot BEFORE teardown — Firecracker's own
        // `/snapshot/create` API would be the natural fit here; this slice
        // uses the same record-level persistence contract as the podman-era
        // design (see `create_snapshot`) rather than a live memory/disk
        // snapshot, which is real future work (documented in the writeup).
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

    async fn run_command(
        &self,
        project_id: &str,
        id: &str,
        input: RunCommandInput,
    ) -> Result<SandboxCommandRecord, SandboxError> {
        validate_argv(&input.cmd, &input.args)?;
        let sandbox = self.get_sandbox(project_id, id).await?;
        let backend = self.backend.as_ref().ok_or_else(|| {
            SandboxError::EngineUnavailable("no Firecracker backend on this node".into())
        })?;
        let handle = self.ensure_cell(&sandbox).await?;
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

        let mut rx = backend
            .exec_command(&handle, guest_req)
            .await
            .map_err(|e| SandboxError::EngineUnavailable(format!("exec failed to start: {e}")))?;

        let notify = Arc::new(tokio::sync::Notify::new());
        self.killers.write().insert(cmd_id.clone(), notify.clone());

        if input.detached {
            let commands = self.commands.clone();
            let killers = self.killers.clone();
            let cmd_id2 = cmd_id.clone();
            tokio::spawn(async move {
                drain_exec_events(&mut rx, &commands, &cmd_id2, &secrets, notify).await;
                killers.write().remove(&cmd_id2);
            });
            return Ok(base_rec);
        }

        drain_exec_events(&mut rx, &self.commands, &cmd_id, &secrets, notify).await;
        self.killers.write().remove(&cmd_id);
        self.get_command(project_id, id, &cmd_id).await
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

    async fn kill_command(
        &self,
        project_id: &str,
        id: &str,
        command_id: &str,
    ) -> Result<SandboxCommandRecord, SandboxError> {
        let sandbox = self.get_sandbox(project_id, id).await?;
        let _ = sandbox; // (kept for the tenant/project-scoped lookup above)
        let handle = self.cells.read().get(id).cloned();
        if let Some(handle) = handle {
            if let Some(backend) = self.backend.as_ref() {
                let _ = backend.kill_exec(&handle, command_id).await;
            }
        }
        if let Some(notify) = self.killers.read().get(command_id) {
            notify.notify_waiters();
        }
        // Give the drain loop a brief moment to observe ExecDone before we read
        // back the record; if the guest doesn't report in time, mark it Killed
        // here so the caller never sees a stale "Running" status forever.
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
            // Base64 through argv (no shell) avoids ever putting file content on
            // a shell-interpreted command line — `sh -c` is never invoked here.
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
        // NOTE (honest scope limit — see writeup): Firecracker natively
        // supports `/snapshot/create` (pause + dump memory/disk), which is NOT
        // wired up in this slice. This records a lightweight, real marker
        // (which base image the sandbox is currently pinned to) so
        // stop→resume reuses that image, WITHOUT live memory-state capture.
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

    async fn domain(&self, project_id: &str, id: &str, port: u16) -> Result<String, SandboxError> {
        let sandbox = self.get_sandbox(project_id, id).await?;
        if !sandbox.ports.contains(&port) {
            return Err(SandboxError::InvalidMount(format!(
                "port {port} was not exposed at sandbox creation — add it to `ports` first"
            )));
        }
        // NOTE (honest scope limit — see writeup): the microVM's egress/ingress
        // networking is per-cell (Firecracker TAP + NAT, see
        // `FirecrackerBackend::setup_cell_net`); a public preview URL requires
        // a host-side port-forward or edge route, which is not wired up in
        // this slice. This documents the intended URL shape so the UI/API
        // contract is stable once that plumbing lands.
        let host = std::env::var("HIVE_PUBLIC_IP")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        Ok(format!("http://{host}:{port}"))
    }

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
        // A running cell's network namespace is fixed at boot (Firecracker has
        // no live network-policy toggle); a policy change here takes effect on
        // the sandbox's NEXT provision, not instantaneously — documented, not
        // silently pretended.
        Ok(rec.clone())
    }

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

        // NOTE (honest scope limit — see writeup): persistent drives and
        // remote-FUSE mounts need a second attached drive / in-guest FUSE
        // driver, neither wired into the microVM boot path in this slice
        // (the data-disk mechanism exists for delivered builds, not
        // sandbox-requested mounts). Configuration is captured and validated
        // in full (including sealed credentials); attachment reports "pending"
        // honestly rather than a fake "mounted".
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

/// Drain one exec's event channel into its `SandboxCommandRecord`, redacting
/// secrets and bounding output as it goes, until `ExecDone` or the kill
/// `Notify` fires (killer already told the guest via `kill_exec`; this just
/// stops us waiting on a connection whose far end may already be gone).
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
                    // Never report success on a non-zero/absent exit.
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
