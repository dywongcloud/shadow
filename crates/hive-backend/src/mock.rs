//! Mock cell backend: a "cell" is a sandboxed child-process build in a temp directory, so the whole
//! control plane runs on macOS without virtualization under the same [`CellBackend`] contract.
//! Isolation is a per-tenant work-dir jail, a wall-clock timeout and best-effort `rlimit`s.

use crate::{
    CellBackend, CellEndpoint, CellHandle, CellSpec, FunctionLaunch, LogSink, SealedRuntimeArtifact,
};
use async_trait::async_trait;
use hive_core::{
    now_ms, BuildJob, BuildResult, CellId, LogLine, LogStream, RuntimeArtifactIdentity,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

/// Backend config knobs.
#[derive(Clone, Debug)]
pub struct MockConfig {
    /// Base directory under which per-cell work dirs are created.
    pub root: PathBuf,
    /// Simulated boot/image-load latency for a *cold* provision. Warm-pool cells
    /// pay this ahead of time, so a job assigned a warm cell never sees it.
    pub provision_latency: Duration,
    /// Shared build cache root (cross-cell, like Netlify's shared cache storage).
    pub cache_root: PathBuf,
    /// Durable directory backing the runtime-artifact receipt sidecar; must survive a `hive-node`
    /// restart, so point it at the sealed artifacts' own durable store, never a tempdir.
    pub receipts_dir: PathBuf,
}

impl Default for MockConfig {
    fn default() -> Self {
        MockConfig {
            root: std::env::temp_dir().join("hive-cells"),
            provision_latency: Duration::from_millis(800),
            cache_root: std::env::temp_dir().join("hive-cache"),
            receipts_dir: std::env::temp_dir().join("hive-cells"),
        }
    }
}

struct RootRollback {
    path: PathBuf,
    armed: bool,
}

impl RootRollback {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for RootRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let path = self.path.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = tokio::fs::remove_dir_all(path).await;
            });
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct MockRuntimeArtifactReceipt {
    identity: RuntimeArtifactIdentity,
    app_root: PathBuf,
}

/// File name of the durable sidecar backing `MockBackend::runtime_artifacts`, so a restart reloads
/// every ready deployment's receipt instead of orphaning it as NODE_IMAGE_MISSING.
const RUNTIME_ARTIFACT_RECEIPTS_FILE: &str = "runtime-artifact-receipts.json";

fn load_runtime_artifact_receipts(
    root: &std::path::Path,
) -> HashMap<String, MockRuntimeArtifactReceipt> {
    let path = root.join(RUNTIME_ARTIFACT_RECEIPTS_FILE);
    let Ok(bytes) = std::fs::read(&path) else {
        return HashMap::new();
    };
    match serde_json::from_slice(&bytes) {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "runtime artifact receipts sidecar unreadable, starting empty"
            );
            HashMap::new()
        }
    }
}

fn save_runtime_artifact_receipts(
    root: &std::path::Path,
    artifacts: &HashMap<String, MockRuntimeArtifactReceipt>,
) {
    let path = root.join(RUNTIME_ARTIFACT_RECEIPTS_FILE);
    let tmp = root.join(format!(
        "{RUNTIME_ARTIFACT_RECEIPTS_FILE}.tmp-{}",
        std::process::id()
    ));
    let Ok(bytes) = serde_json::to_vec(artifacts) else {
        return;
    };
    if std::fs::create_dir_all(root).is_err() {
        return;
    }
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

pub struct MockBackend {
    cfg: MockConfig,
    /// Exact immutable application bytes authorized for each platform image id.
    runtime_artifacts: Arc<std::sync::Mutex<HashMap<String, MockRuntimeArtifactReceipt>>>,
    /// Long-lived function processes, keyed by cell, killed on terminate.
    funcs: Arc<AsyncMutex<HashMap<CellId, tokio::process::Child>>>,
    /// Per-cell tunnel-server accept loops, aborted on terminate.
    tunnels: Arc<AsyncMutex<HashMap<CellId, tokio::task::JoinHandle<()>>>>,
    /// Per-cell host-container ownership: exact container identity and tunnel task.
    containers: Arc<AsyncMutex<HashMap<CellId, crate::ContainerLaunch>>>,
    /// Throttled batch CPU sampler for `cpu_percent` (#2).
    sampler: Arc<crate::CpuSampler>,
    /// Native macOS litebox: config + per-cell net identity (`terminate` kills the real runner process
    /// with it). See `crate::litebox_macos`.
    litebox_macos_cfg: crate::litebox_macos::LiteboxMacosConfig,
    litebox_macos_net: Arc<crate::litebox_macos::NetAllocator>,
    litebox_macos_cells: Arc<AsyncMutex<HashMap<CellId, crate::litebox_macos::MacosNet>>>,
}

impl MockBackend {
    pub fn new(cfg: MockConfig) -> Self {
        let restored = load_runtime_artifact_receipts(&cfg.receipts_dir);
        if !restored.is_empty() {
            tracing::info!(
                count = restored.len(),
                "restored runtime artifact receipts from disk"
            );
        }
        MockBackend {
            cfg,
            runtime_artifacts: Arc::new(std::sync::Mutex::new(restored)),
            funcs: Arc::new(AsyncMutex::new(HashMap::new())),
            tunnels: Arc::new(AsyncMutex::new(HashMap::new())),
            containers: Arc::new(AsyncMutex::new(HashMap::new())),
            sampler: Arc::new(crate::CpuSampler::new()),
            litebox_macos_cfg: crate::litebox_macos::LiteboxMacosConfig::default(),
            litebox_macos_net: Arc::new(crate::litebox_macos::NetAllocator::new()),
            litebox_macos_cells: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        MockBackend::new(MockConfig::default())
    }
}

/// Run a build job as a plain host process — the un-sandboxed build pipeline shared by
/// [`MockBackend`] and the Litebox backend, since a build is fork/exec-heavy and litebox's
/// `sys_clone` does not support `fork`; litebox's isolation is scoped to `start_function`.
pub(crate) async fn run_build_process(
    cell: &CellHandle,
    job: &BuildJob,
    sink: LogSink,
    cache_root: &PathBuf,
) -> anyhow::Result<BuildResult> {
    let started_at_ms = now_ms();
    sys_log(
        &sink,
        format!(
            "[{}] starting build {} (image={}, {}vcpu/{}MiB)",
            cell.id, job.id, cell.image, cell.resources.vcpus, cell.resources.mem_mib
        ),
    );

    let mut steps: Vec<String> = Vec::new();
    if !job.repo.is_empty() {
        steps.push(format!(
            "git clone --depth 1 {} {} . 2>&1 || (echo 'clone failed' && exit 1)",
            job.git_ref_clone_arg(),
            shell_quote(&job.repo)
        ));
    }
    steps.extend(job.commands.iter().cloned());

    let mem_mib = cell.resources.mem_mib;
    let timeout = Duration::from_secs(job.resources.timeout_secs.max(1));

    if let Some(key) = &job.cache_key {
        if !job.cache_paths.is_empty() {
            let summary = restore_cache(cache_root, key, &job.cache_paths, &cell.root);
            sys_log(&sink, format!("build cache restore [{key}]: {summary}"));
        }
    }

    let run = async {
        let mut last_code = 0i32;
        for (i, step) in steps.iter().enumerate() {
            sys_log(&sink, format!("$ {}", step));
            let code = run_step(&cell.root, step, &job.env, mem_mib, &sink, i).await?;
            last_code = code;
            if code != 0 {
                break;
            }
        }
        anyhow::Ok(last_code)
    };

    let (exit_code, timed_out) = match tokio::time::timeout(timeout, run).await {
        Ok(Ok(code)) => (code, false),
        Ok(Err(e)) => {
            sys_log(&sink, format!("build error: {e}"));
            (-1, false)
        }
        Err(_) => {
            sys_log(&sink, format!("build exceeded timeout of {timeout:?}"));
            (-1, true)
        }
    };

    if exit_code == 0 && !timed_out {
        if let Some(key) = &job.cache_key {
            if !job.cache_paths.is_empty() {
                let summary = save_cache(cache_root, key, &job.cache_paths, &cell.root);
                sys_log(&sink, format!("build cache save [{key}]: {summary}"));
            }
        }
    }

    let finished_at_ms = now_ms();
    sys_log(
        &sink,
        format!(
            "[{}] build {} finished: exit={} timed_out={} ({}ms)",
            cell.id,
            job.id,
            exit_code,
            timed_out,
            finished_at_ms.saturating_sub(started_at_ms)
        ),
    );

    Ok(BuildResult {
        job_id: job.id.clone(),
        exit_code,
        timed_out,
        started_at_ms,
        finished_at_ms,
    })
}

/// Is `prog` executable via `path` — the SAME `PATH` handed to the child; a program containing a
/// separator is tested directly (`execvp` semantics).
fn bin_on_path(prog: &str, path: &str) -> bool {
    if prog.contains('/') {
        return std::path::Path::new(prog).is_file();
    }
    path.split(':')
        .filter(|d| !d.is_empty())
        .any(|d| std::path::Path::new(d).join(prog).is_file())
}

fn sys_log(sink: &LogSink, line: impl Into<String>) {
    let _ = sink.send(LogLine {
        ts_ms: now_ms(),
        stream: LogStream::System,
        line: line.into(),
    });
}

#[async_trait]
impl CellBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn requires_runtime_artifact_authorization(&self) -> bool {
        true
    }

    async fn deliver_build(
        &self,
        image: &str,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<()> {
        let receipt = MockRuntimeArtifactReceipt {
            identity: artifact.identity(image)?,
            app_root: artifact.host_app_root()?,
        };
        let snapshot = {
            let mut artifacts = self
                .runtime_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = artifacts.get(image) {
                anyhow::ensure!(
                    existing == &receipt,
                    "runtime artifact image id {image:?} is already bound to different immutable content"
                );
                return Ok(());
            }
            artifacts.insert(image.to_string(), receipt);
            artifacts.clone()
        };
        save_runtime_artifact_receipts(&self.cfg.receipts_dir, &snapshot);
        Ok(())
    }

    async fn runtime_artifact_identity(
        &self,
        image: &str,
    ) -> anyhow::Result<Option<RuntimeArtifactIdentity>> {
        let artifacts = self
            .runtime_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let receipt = artifacts.get(image).ok_or_else(|| {
            anyhow::anyhow!(
                "node is missing the committed runtime artifact for {image:?} ({})",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        Ok(Some(receipt.identity.clone()))
    }

    async fn provision_runtime(
        &self,
        spec: &CellSpec,
        expected: Option<&RuntimeArtifactIdentity>,
    ) -> anyhow::Result<CellHandle> {
        if let Some(expected) = expected {
            anyhow::ensure!(
                spec.container.is_none() && spec.image.starts_with("dpl-"),
                "runtime artifact authorization was presented for a non-artifact cell {} ({})",
                spec.id,
                hive_core::fault::NODE_IMAGE_MISSING
            );
            let artifacts = self
                .runtime_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let receipt = artifacts.get(&spec.image).ok_or_else(|| {
                anyhow::anyhow!(
                    "node is missing the committed runtime artifact for {:?} ({})",
                    spec.image,
                    hive_core::fault::NODE_IMAGE_MISSING
                )
            })?;
            anyhow::ensure!(
                &receipt.identity == expected,
                "deployment {:?} artifact changed after caller authorization ({})",
                spec.image,
                hive_core::fault::NODE_IMAGE_MISSING
            );
        }
        self.provision(spec).await
    }

    async fn provision(&self, spec: &CellSpec) -> anyhow::Result<CellHandle> {
        let tenant = if spec.tenant.trim().is_empty() {
            "personal"
        } else {
            spec.tenant.as_str()
        };
        let root = self
            .cfg
            .root
            .join(crate::sanitize_tenant(tenant))
            .join(spec.id.as_str());
        let mut provision = RootRollback::new(root.clone());
        tokio::fs::create_dir_all(&root).await?;
        tokio::time::sleep(self.cfg.provision_latency).await;
        provision.commit();
        Ok(CellHandle {
            id: spec.id.clone(),
            image: spec.image.clone(),
            resources: spec.resources.clone(),
            root,
            endpoint: None,
        })
    }

    async fn run_build(
        &self,
        cell: &CellHandle,
        job: &BuildJob,
        sink: LogSink,
    ) -> anyhow::Result<BuildResult> {
        run_build_process(cell, job, sink, &self.cfg.cache_root).await
    }

    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint> {
        anyhow::ensure!(!func.start_cmd.is_empty(), "empty function start_cmd");

        if func.start_cmd[0] == "__container__" {
            let image = func.start_cmd.get(1).cloned().unwrap_or_default();
            let internal = func
                .start_cmd
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(8080);
            let net_json = func
                .start_cmd
                .get(3)
                .map(String::as_str)
                .filter(|s| !s.is_empty());
            let mut ports = vec![crate::ContainerPort::tcp(internal, func.port)];
            ports.extend(func.udp_ports.iter().map(|u| crate::ContainerPort {
                container_port: u.container_port,
                host_port: u.host_port,
                protocol: crate::ContainerProtocol::Udp,
            }));
            ports.extend(func.tcp_ports.iter().filter_map(|t| {
                (t.host_port != func.port).then_some(crate::ContainerPort {
                    container_port: t.container_port,
                    host_port: t.host_port,
                    protocol: crate::ContainerProtocol::Tcp,
                })
            }));
            let runtime = crate::container_runtime();
            let launch = crate::podman_run_container(
                &cell.id,
                &image,
                &ports,
                &func.env,
                func.max_concurrency,
                "/opt/homebrew/bin:/usr/local/bin:/usr/local/sbin:/usr/bin:/usr/sbin:/bin:/sbin",
                runtime.as_deref(),
                net_json,
                &crate::ContainerLimits::for_container(func.memory_mib, func.cpus, func.pids),
                func.raw_proxy,
                func.gpu,
            )
            .await?;
            let endpoint = launch.endpoint();
            self.containers.lock().await.insert(cell.id.clone(), launch);
            return Ok(endpoint);
        }

        let workdir = func.workdir.as_ref().map(PathBuf::from).ok_or_else(|| {
            anyhow::anyhow!(
                "mock function launch omitted its runtime workdir ({})",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        if cell.image.starts_with("dpl-") {
            let authorized = func.runtime_artifact.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "mock function launch omitted the caller-authorized runtime artifact identity ({})",
                    hive_core::fault::NODE_IMAGE_MISSING
                )
            })?;
            let artifacts = self
                .runtime_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let receipt = artifacts.get(&cell.image).ok_or_else(|| {
                anyhow::anyhow!(
                    "node is missing the committed runtime artifact for {:?} ({})",
                    cell.image,
                    hive_core::fault::NODE_IMAGE_MISSING
                )
            })?;
            anyhow::ensure!(
                &receipt.identity == authorized && receipt.app_root == workdir,
                "mock function launch does not match its committed sealed runtime artifact ({})",
                hive_core::fault::NODE_IMAGE_MISSING
            );
        } else {
            anyhow::ensure!(
                func.runtime_artifact.is_none(),
                "non-artifact mock image {:?} received a runtime artifact identity ({})",
                cell.image,
                hive_core::fault::NODE_IMAGE_MISSING
            );
        }

        if crate::litebox_macos::eligible(func.runtime)
            && crate::litebox_macos::available(&self.litebox_macos_cfg).await
        {
            return self.start_function_litebox_macos(cell, func).await;
        }

        let base_path = std::env::var("PATH").unwrap_or_default();
        let path = format!("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{base_path}");
        let joined = func.start_cmd.join(" ");
        let is_container = joined.contains("podman")
            || joined.contains("docker")
            || joined.contains("container run ");
        let is_wasm = func.runtime == hive_core::Runtime::Wasmer;
        let ready_timeout = if is_container || is_wasm { 60 } else { 15 };

        if is_wasm && !bin_on_path(&func.start_cmd[0], &path) {
            anyhow::bail!(
                "{}: this node has no `{}` binary on PATH, so a runtime=\"wasmer\" \
                 deployment cannot start here — install the wasmer CLI on this node \
                 (operator remedy; not an application fault)",
                hive_core::fault::NODE_RUNTIME_MISSING,
                func.start_cmd[0],
            );
        }

        let is_bun = func.runtime == hive_core::Runtime::Bun;
        if is_bun && !bin_on_path(&func.start_cmd[0], &path) {
            anyhow::bail!(
                "{}: this node has no `{}` binary on PATH, so a runtime=\"bun\" \
                 deployment cannot start here — install the bun CLI on this node \
                 (operator remedy; not an application fault)",
                hive_core::fault::NODE_RUNTIME_MISSING,
                func.start_cmd[0],
            );
        }

        let runtime_home = cell.root.join("runtime-home");
        let runtime_tmp = cell.root.join("runtime-tmp");
        let runtime_cache = cell.root.join("runtime-cache");
        tokio::fs::create_dir_all(&runtime_home).await?;
        tokio::fs::create_dir_all(&runtime_tmp).await?;
        tokio::fs::create_dir_all(&runtime_cache).await?;

        let mut cmd = Command::new(&func.start_cmd[0]);
        cmd.env_clear()
            .args(&func.start_cmd[1..])
            .current_dir(&workdir)
            .env("PORT", func.port.to_string())
            .env("PATH", path)
            .env("HOME", &runtime_home)
            .env("TMPDIR", &runtime_tmp)
            .env("XDG_CACHE_HOME", &runtime_cache)
            .envs(&func.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let cc_off = func
            .env
            .get("HIVE_COMPILE_CACHE")
            .map(|v| v == "0" || v == "false")
            .unwrap_or(false);
        if !cc_off && func.runtime.uses_v8_compile_cache() {
            cmd.env("NODE_COMPILE_CACHE", &runtime_cache);
        }

        let child = cmd.spawn()?;
        self.funcs.lock().await.insert(cell.id.clone(), child);

        let func_addr = format!("127.0.0.1:{}", func.port);
        wait_tcp_ready(&func_addr, Duration::from_secs(ready_timeout)).await?;

        let raw_proxy = func.raw_proxy;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let tunnel_addr = listener.local_addr()?.to_string();
        let max_conc = func.max_concurrency.max(1);
        let func_addr_for_task = func_addr.clone();
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((conn, _)) => {
                        let local = func_addr_for_task.clone();
                        tokio::spawn(async move {
                            if raw_proxy {
                                fluid_tunnel::TunnelServer::serve_raw(conn, local).await;
                            } else {
                                fluid_tunnel::TunnelServer::serve_maybe_raw(conn, local, max_conc)
                                    .await;
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });
        let task = crate::AbortTask::new(task);
        let mut tunnels = self.tunnels.lock().await;
        if let Some(task) = task.publish() {
            tunnels.insert(cell.id.clone(), task);
        }

        Ok(CellEndpoint::Tcp(tunnel_addr))
    }

    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()> {
        let id = cell.id.clone();
        let root = cell.root.clone();
        let tunnels = self.tunnels.clone();
        let containers = self.containers.clone();
        let funcs = self.funcs.clone();
        let macos_cells = self.litebox_macos_cells.clone();
        let macos_cfg = self.litebox_macos_cfg.clone();
        let cleanup = tokio::spawn(async move {
            let tunnel = tunnels.lock().await.remove(&id);
            if let Some(task) = tunnel {
                task.abort();
            }
            let container = containers.lock().await.remove(&id);
            if let Some(container) = container {
                container.terminate().await;
            }
            let process = funcs.lock().await.remove(&id);
            if let Some(mut child) = process {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            let macos_net = macos_cells.lock().await.remove(&id);
            if let Some(net) = macos_net {
                crate::litebox_macos::kill_runner(&macos_cfg, &net).await;
            }
            let _ = tokio::fs::remove_dir_all(root).await;
        });
        cleanup
            .await
            .map_err(|e| anyhow::anyhow!("mock cleanup task failed: {e}"))
    }

    async fn cpu_percent(&self, cell: &CellHandle) -> Option<f32> {
        let pid = {
            let funcs = self.funcs.lock().await;
            funcs.get(&cell.id).and_then(|c| c.id())?
        };
        self.sampler.cpu_percent(pid, cell.resources.vcpus)
    }
}

impl MockBackend {
    /// Start a Node function inside the native macOS litebox runner; only called once
    /// `litebox_macos::available` confirmed the grant, so failures past this point surface as
    /// `DEPLOYMENT_START_FAILED`, never a silent fallback to an unsandboxed process.
    async fn start_function_litebox_macos(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint> {
        let workdir = func
            .workdir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| cell.root.clone());

        let base_rootfs = std::env::var("HIVE_LITEBOX_MACOS_ROOTFS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| self.cfg.root.join("litebox-macos-rootfs.tar"));
        anyhow::ensure!(
            base_rootfs.exists(),
            "{}: litebox-macos: no base rootfs staged at {} (set HIVE_LITEBOX_MACOS_ROOTFS or \
             place the Node+npm bundle there)",
            hive_core::fault::NODE_RUNTIME_MISSING,
            base_rootfs.display()
        );

        let net = self.litebox_macos_net.next();
        let tar_dest = self
            .cfg
            .root
            .join("litebox-macos-tars")
            .join(format!("{}.tar", cell.id.as_str()));
        crate::litebox_macos::build_combined_tar(&base_rootfs, &workdir, &tar_dest).await?;

        let guest_path =
            "/usr/local/bin:/usr/local/lib/node_modules/npm/bin:/bin:/usr/bin:./node_modules/.bin";
        let script = func
            .start_cmd
            .iter()
            .map(|s| shell_quote(s))
            .collect::<Vec<_>>()
            .join(" ");

        let mut env = func.env.clone();
        env.insert("PORT".into(), func.port.to_string());
        env.insert("PATH".into(), guest_path.into());
        env.insert("HOME".into(), "/".into());

        self.litebox_macos_cells
            .lock()
            .await
            .insert(cell.id.clone(), net.clone());
        let child = crate::litebox_macos::start(
            &self.litebox_macos_cfg,
            &net,
            &tar_dest,
            "/bin/sh",
            &["-c".to_string(), script],
            &env,
        )
        .await?;

        self.funcs.lock().await.insert(cell.id.clone(), child);

        let func_addr = format!("{}:{}", net.guest_ip, func.port);
        wait_tcp_ready(&func_addr, Duration::from_secs(60)).await?;

        let raw_proxy = func.raw_proxy;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let tunnel_addr = listener.local_addr()?.to_string();
        let max_conc = func.max_concurrency.max(1);
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((conn, _)) => {
                        let local = func_addr.clone();
                        tokio::spawn(async move {
                            if raw_proxy {
                                fluid_tunnel::TunnelServer::serve_raw(conn, local).await;
                            } else {
                                fluid_tunnel::TunnelServer::serve_maybe_raw(conn, local, max_conc)
                                    .await;
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });
        let task = crate::AbortTask::new(task);
        let mut tunnels = self.tunnels.lock().await;
        if let Some(task) = task.publish() {
            tunnels.insert(cell.id.clone(), task);
        }

        Ok(CellEndpoint::Tcp(tunnel_addr))
    }
}

pub(crate) async fn wait_tcp_ready(addr: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::from("unknown");
    while tokio::time::Instant::now() < deadline {
        let per_attempt = Duration::from_secs(2)
            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        match tokio::time::timeout(per_attempt, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(e)) => {
                last = e.to_string();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => {
                last = format!("connect attempt exceeded {per_attempt:?}");
            }
        }
    }
    anyhow::bail!(
        "the deployment's own process never listened on {addr} within {}s — check this \
         deployment's logs, entrypoint and required env; the node started the cell fine ({}). \
         Last connect error: {last}",
        timeout.as_secs(),
        hive_core::fault::DEPLOYMENT_START_FAILED
    )
}

/// Run a single shell step, streaming stdout/stderr lines to `sink`.
async fn run_step(
    cwd: &PathBuf,
    step: &str,
    env: &std::collections::BTreeMap<String, String>,
    mem_mib: u32,
    sink: &LogSink,
    _idx: usize,
) -> anyhow::Result<i32> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(step)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", cwd)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    apply_rlimits(&mut cmd, mem_mib);

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let s_out = sink.clone();
    let out_task = tokio::spawn(async move {
        let mut r = BufReader::new(stdout);
        while let Ok(Some(l)) =
            hive_core::logcap::read_capped_line(&mut r, hive_core::MAX_LOG_LINE_BYTES).await
        {
            let _ = s_out.send(LogLine {
                ts_ms: now_ms(),
                stream: LogStream::Stdout,
                line: l.text,
            });
        }
    });

    let s_err = sink.clone();
    let err_task = tokio::spawn(async move {
        let mut r = BufReader::new(stderr);
        while let Ok(Some(l)) =
            hive_core::logcap::read_capped_line(&mut r, hive_core::MAX_LOG_LINE_BYTES).await
        {
            let _ = s_err.send(LogLine {
                ts_ms: now_ms(),
                stream: LogStream::Stderr,
                line: l.text,
            });
        }
    });

    let status = child.wait().await?;
    let _ = out_task.await;
    let _ = err_task.await;
    Ok(status.code().unwrap_or(-1))
}

/// Best-effort resource limits via `setrlimit` in the forked child, before exec.
/// No-op on non-Unix. Caps address space (a rough memory ceiling) and CPU time.
#[cfg(unix)]
fn apply_rlimits(cmd: &mut Command, mem_mib: u32) {
    let mem_bytes = (mem_mib as u64).saturating_mul(1024 * 1024);
    unsafe {
        cmd.pre_exec(move || {
            let lim = libc::rlimit {
                rlim_cur: mem_bytes,
                rlim_max: mem_bytes,
            };
            let _ = libc::setrlimit(libc::RLIMIT_AS, &lim);
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn apply_rlimits(_cmd: &mut Command, _mem_mib: u32) {}

/// Minimal shell quoting for the assembled clone command.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'@'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Restore cached `paths` from `cache_root/key/<path>` into `dest/<path>`.
fn restore_cache(cache_root: &PathBuf, key: &str, paths: &[String], dest: &PathBuf) -> String {
    let mut hits = Vec::new();
    for p in paths {
        let src = cache_root.join(key).join(p);
        let dst = dest.join(p);
        if src.exists() && copy_dir_all(&src, &dst).is_ok() {
            hits.push(p.clone());
        }
    }
    if hits.is_empty() {
        "miss".to_string()
    } else {
        format!("hit [{}]", hits.join(", "))
    }
}

/// Save `paths` from `dest/<path>` into `cache_root/key/<path>`.
fn save_cache(cache_root: &PathBuf, key: &str, paths: &[String], dest: &PathBuf) -> String {
    let mut saved = Vec::new();
    for p in paths {
        let src = dest.join(p);
        let dst = cache_root.join(key).join(p);
        if src.exists() {
            let _ = std::fs::remove_dir_all(&dst);
            if copy_dir_all(&src, &dst).is_ok() {
                saved.push(p.clone());
            }
        }
    }
    if saved.is_empty() {
        "nothing to save".to_string()
    } else {
        format!("saved [{}]", saved.join(", "))
    }
}

/// Recursive directory copy (creates parents). Files are copied as-is.
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

trait GitRefArg {
    fn git_ref_clone_arg(&self) -> String;
}
impl GitRefArg for BuildJob {
    fn git_ref_clone_arg(&self) -> String {
        if self.git_ref.is_empty() || self.git_ref == "HEAD" {
            String::new()
        } else {
            format!("--branch {}", self.git_ref)
        }
    }
}

#[cfg(test)]
mod tenant_tests {
    use super::*;
    use hive_core::ResourceSpec;

    fn spec(tenant: &str) -> CellSpec {
        CellSpec {
            id: CellId::new(),
            image: "img".into(),
            resources: ResourceSpec {
                vcpus: 1,
                mem_mib: 64,
                disk_mib: 64,
                timeout_secs: 0,
            },
            tenant: tenant.into(),
            container: None,
        }
    }

    /// Every cell's sandbox lives under its tenant's subtree, so two tenants'
    /// cells are on disjoint host paths; an empty tenant normalizes to "personal".
    #[tokio::test]
    async fn provision_isolates_cell_workdirs_by_tenant() {
        let root = std::env::temp_dir().join(format!("mock-tenant-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let be = MockBackend::new(MockConfig {
            root: root.clone(),
            provision_latency: Duration::from_millis(0),
            cache_root: root.join("cache"),
            receipts_dir: root.join("cache"),
        });

        let a = be.provision(&spec("alpha")).await.unwrap();
        let b = be.provision(&spec("beta")).await.unwrap();
        let p = be.provision(&spec("")).await.unwrap();

        assert!(
            a.root.starts_with(root.join("alpha")),
            "alpha cell not under its tenant dir: {:?}",
            a.root
        );
        assert!(
            b.root.starts_with(root.join("beta")),
            "beta cell not under its tenant dir: {:?}",
            b.root
        );
        assert!(
            p.root.starts_with(root.join("personal")),
            "empty tenant should map to personal: {:?}",
            p.root
        );
        assert_ne!(
            a.root.parent(),
            b.root.parent(),
            "tenants must not share a cell parent dir"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// #2: `cpu_percent` reports REAL CPU — a busy process pegs near a full core,
    /// an idle one stays low. This is the genuine saturation signal the adaptive
    /// concurrency controller consumes (no latency proxy).
    #[tokio::test]
    async fn cpu_percent_reflects_real_process_load() {
        let be = MockBackend::default();
        let handle = |id: &CellId| CellHandle {
            id: id.clone(),
            image: "x".into(),
            resources: ResourceSpec {
                vcpus: 1,
                mem_mib: 64,
                disk_mib: 64,
                timeout_secs: 0,
            },
            root: std::env::temp_dir(),
            endpoint: None,
        };

        // Busy child: a shell spin loop pegs one core.
        let busy_id = CellId::new();
        let busy = Command::new("sh")
            .arg("-c")
            .arg("while :; do :; done")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        be.funcs.lock().await.insert(busy_id.clone(), busy);

        // Idle child: sleeps, ~0 CPU.
        let idle_id = CellId::new();
        let idle = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        be.funcs.lock().await.insert(idle_id.clone(), idle);

        // Prime the sampler (first sample is the baseline → 0), then measure.
        let _ = be.cpu_percent(&handle(&busy_id)).await;
        let _ = be.cpu_percent(&handle(&idle_id)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let busy_cpu = be.cpu_percent(&handle(&busy_id)).await.unwrap_or(0.0);
        let idle_cpu = be.cpu_percent(&handle(&idle_id)).await.unwrap_or(-1.0);

        be.terminate(&handle(&busy_id)).await.unwrap();
        be.terminate(&handle(&idle_id)).await.unwrap();

        assert!(
            busy_cpu > 40.0,
            "busy process should report high CPU, got {busy_cpu}"
        );
        assert!(
            idle_cpu >= 0.0 && idle_cpu < 25.0,
            "idle process should report low CPU, got {idle_cpu}"
        );
        assert!(
            busy_cpu > idle_cpu,
            "busy must exceed idle ({busy_cpu} vs {idle_cpu})"
        );
    }

    /// A hostile tenant slug can't escape the cells root via path traversal.
    #[tokio::test]
    async fn provision_sanitizes_traversal_in_tenant() {
        let root = std::env::temp_dir().join(format!("mock-traversal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let be = MockBackend::new(MockConfig {
            root: root.clone(),
            provision_latency: Duration::from_millis(0),
            cache_root: root.join("cache"),
            receipts_dir: root.join("cache"),
        });
        let h = be.provision(&spec("../../etc")).await.unwrap();
        assert!(
            h.root.starts_with(&root),
            "tenant slug escaped the cells root: {:?}",
            h.root
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
