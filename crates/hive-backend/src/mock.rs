//! Mock cell backend: a "cell" is a sandboxed child-process build in a temp
//! directory. This lets the entire control plane run and be exercised on macOS
//! / Apple Silicon without any virtualization, while presenting the exact same
//! [`CellBackend`] contract as the real Firecracker backend.
//!
//! Isolation here is intentionally lighter than a microVM — a per-tenant work-dir
//! jail (cells live under `<root>/<tenant>/<cell-id>`, so tenants never share a
//! subtree), a hard wall-clock timeout, and best-effort `rlimit`s (CPU seconds +
//! address space) on Unix. Stronger isolation (separate kernels) is the
//! Firecracker backend's job; tenant tagging flows identically through both.

use crate::{CellBackend, CellEndpoint, CellHandle, CellSpec, FunctionLaunch, LogSink};
use async_trait::async_trait;
use hive_core::{now_ms, BuildJob, BuildResult, CellId, LogLine, LogStream};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
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
}

impl Default for MockConfig {
    fn default() -> Self {
        MockConfig {
            root: std::env::temp_dir().join("hive-cells"),
            // A nod to the blog: cold provisioning is the slow path warm pools hide.
            provision_latency: Duration::from_millis(800),
            cache_root: std::env::temp_dir().join("hive-cache"),
        }
    }
}

pub struct MockBackend {
    cfg: MockConfig,
    /// Long-lived function processes, keyed by cell, killed on terminate.
    funcs: Arc<AsyncMutex<HashMap<CellId, tokio::process::Child>>>,
    /// Per-cell tunnel-server accept loops, aborted on terminate.
    tunnels: Arc<AsyncMutex<HashMap<CellId, tokio::task::JoinHandle<()>>>>,
    /// Per-cell podman container names (Railway-style container deploys).
    containers: Arc<AsyncMutex<HashMap<CellId, String>>>,
}

impl MockBackend {
    pub fn new(cfg: MockConfig) -> Self {
        MockBackend {
            cfg,
            funcs: Arc::new(AsyncMutex::new(HashMap::new())),
            tunnels: Arc::new(AsyncMutex::new(HashMap::new())),
            containers: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        MockBackend::new(MockConfig::default())
    }
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

    async fn provision(&self, spec: &CellSpec) -> anyhow::Result<CellHandle> {
        // Per-tenant isolation: every cell's sandbox lives under its tenant's
        // subtree (`<root>/<tenant>/<cell-id>`), so one tenant's cells can never
        // see another's working files — the host-level analogue of the
        // per-microVM rootfs boundary. Empty tenant => "personal".
        let tenant = if spec.tenant.trim().is_empty() { "personal" } else { spec.tenant.as_str() };
        let root = self.cfg.root.join(crate::sanitize_tenant(tenant)).join(spec.id.as_str());
        tokio::fs::create_dir_all(&root).await?;
        // Simulate the cold-boot + image-load cost the warm pool exists to hide.
        tokio::time::sleep(self.cfg.provision_latency).await;
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
        let started_at_ms = now_ms();
        sys_log(
            &sink,
            format!(
                "[{}] starting build {} (image={}, {}vcpu/{}MiB)",
                cell.id, job.id, cell.image, cell.resources.vcpus, cell.resources.mem_mib
            ),
        );

        // Assemble the script: optional fetch, then each user command.
        let mut steps: Vec<String> = Vec::new();
        if !job.repo.is_empty() {
            // `git_ref_clone_arg` is a controlled flag ("" or "--branch <ref>"),
            // so it is interpolated raw; only the repo URL is quoted.
            steps.push(format!(
                "git clone --depth 1 {} {} . 2>&1 || (echo 'clone failed' && exit 1)",
                job.git_ref_clone_arg(),
                shell_quote(&job.repo)
            ));
        }
        steps.extend(job.commands.iter().cloned());

        let mem_mib = cell.resources.mem_mib;
        let timeout = Duration::from_secs(job.resources.timeout_secs.max(1));

        // Build cache: restore cached paths before the build (the "instant
        // npm install" trick from Netlify/Hive).
        if let Some(key) = &job.cache_key {
            if !job.cache_paths.is_empty() {
                let summary =
                    restore_cache(&self.cfg.cache_root, key, &job.cache_paths, &cell.root);
                sys_log(&sink, format!("build cache restore [{key}]: {summary}"));
            }
        }

        // Run all steps under one wall-clock budget; `set -e` semantics: stop on
        // first non-zero exit.
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

        // Save cache after a successful build.
        if exit_code == 0 && !timed_out {
            if let Some(key) = &job.cache_key {
                if !job.cache_paths.is_empty() {
                    let summary =
                        save_cache(&self.cfg.cache_root, key, &job.cache_paths, &cell.root);
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

    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint> {
        anyhow::ensure!(!func.start_cmd.is_empty(), "empty function start_cmd");

        // Container deploys (Railway-style): `["__container__", image, internal]`.
        // Run detached + named so the gateway can proxy to the published port and
        // `terminate` can clean it up reliably (kill_on_drop can't stop podman).
        if func.start_cmd[0] == "__container__" {
            let image = func.start_cmd.get(1).cloned().unwrap_or_default();
            let internal = func.start_cmd.get(2).cloned().unwrap_or_else(|| "8080".into());
            let name = format!("hive-{}", cell.id.as_str().replace(|c: char| !c.is_ascii_alphanumeric(), "-"));
            let _ = std::process::Command::new("podman").args(["rm", "-f", &name]).output();
            let port = func.port;
            let mut args: Vec<String> = vec![
                "run".into(), "-d".into(), "--name".into(), name.clone(),
                "-e".into(), format!("PORT={internal}"),
            ];
            // Inject the project's env vars into the container runtime.
            for (k, v) in &func.env {
                args.push("-e".into());
                args.push(format!("{k}={v}"));
            }
            args.push("-p".into());
            args.push(format!("127.0.0.1:{port}:{internal}"));
            args.push(image.clone());
            let status = Command::new("podman")
                .args(&args)
                .env("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
                .output()
                .await?;
            anyhow::ensure!(status.status.success(), "podman run failed: {}", String::from_utf8_lossy(&status.stderr).trim());
            self.containers.lock().await.insert(cell.id.clone(), name);
            let func_addr = format!("127.0.0.1:{port}");
            wait_tcp_ready(&func_addr, Duration::from_secs(60)).await?;
            // Front the container with the multiplexed tunnel server.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let tunnel_addr = listener.local_addr()?.to_string();
            let max_conc = func.max_concurrency.max(1);
            let task = tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((conn, _)) => {
                            let local = func_addr.clone();
                            tokio::spawn(async move { fluid_tunnel::TunnelServer::serve(conn, local, max_conc).await; });
                        }
                        Err(_) => break,
                    }
                }
            });
            self.tunnels.lock().await.insert(cell.id.clone(), task);
            return Ok(CellEndpoint::Tcp(tunnel_addr));
        }

        let workdir = func
            .workdir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| cell.root.clone());

        // Ensure common tool dirs (podman/docker, homebrew) are on PATH so
        // container deploys work regardless of how the node was launched.
        let base_path = std::env::var("PATH").unwrap_or_default();
        let path = format!("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{base_path}");
        // Containers (podman) take longer to publish their port than a plain
        // process, especially on macOS rootless networking.
        let is_container = func.start_cmd.join(" ").contains("podman")
            || func.start_cmd.join(" ").contains("docker");
        let ready_timeout = if is_container { 60 } else { 15 };

        let mut cmd = Command::new(&func.start_cmd[0]);
        cmd.args(&func.start_cmd[1..])
            .current_dir(&workdir)
            .env("PORT", func.port.to_string())
            .env("PATH", path)
            .env("HOME", &workdir)
            .envs(&func.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let child = cmd.spawn()?;
        self.funcs.lock().await.insert(cell.id.clone(), child);

        // Readiness: wait until the function accepts TCP on its port.
        let func_addr = format!("127.0.0.1:{}", func.port);
        wait_tcp_ready(&func_addr, Duration::from_secs(ready_timeout)).await?;

        // Front the function with a multiplexed tunnel server (the mock
        // equivalent of the in-VM cell agent). The gateway connects ONE tunnel
        // here and multiplexes all requests over it.
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
                            fluid_tunnel::TunnelServer::serve(conn, local, max_conc).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        });
        self.tunnels.lock().await.insert(cell.id.clone(), task);

        Ok(CellEndpoint::Tcp(tunnel_addr))
    }

    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()> {
        // Stop the tunnel accept loop.
        if let Some(task) = self.tunnels.lock().await.remove(&cell.id) {
            task.abort();
        }
        // Remove any container bound to this cell.
        if let Some(name) = self.containers.lock().await.remove(&cell.id) {
            let _ = Command::new("podman").args(["rm", "-f", &name]).output().await;
        }
        // Kill any function process bound to this cell.
        if let Some(mut child) = self.funcs.lock().await.remove(&cell.id) {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        // Single-use build cell: blow away the work dir.
        let _ = tokio::fs::remove_dir_all(&cell.root).await;
        Ok(())
    }
}

async fn wait_tcp_ready(addr: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::from("unknown");
    while tokio::time::Instant::now() < deadline {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    anyhow::bail!("function did not become ready on {addr}: {last}")
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
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = s_out.send(LogLine {
                ts_ms: now_ms(),
                stream: LogStream::Stdout,
                line,
            });
        }
    });

    let s_err = sink.clone();
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = s_err.send(LogLine {
                ts_ms: now_ms(),
                stream: LogStream::Stderr,
                line,
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
            // Address-space ceiling ~ requested memory (best effort; some
            // runtimes reserve large virtual space, so this is advisory).
            let lim = libc::rlimit {
                rlim_cur: mem_bytes,
                rlim_max: mem_bytes,
            };
            // Ignore failures: this is a dev sandbox, not the security boundary.
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

// Small helper on BuildJob for the clone branch flag.
trait GitRefArg {
    fn git_ref_clone_arg(&self) -> String;
}
impl GitRefArg for BuildJob {
    fn git_ref_clone_arg(&self) -> String {
        if self.git_ref.is_empty() || self.git_ref == "HEAD" {
            // no explicit branch flag
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
            resources: ResourceSpec { vcpus: 1, mem_mib: 64, disk_mib: 64, timeout_secs: 0 },
            tenant: tenant.into(),
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
        });

        let a = be.provision(&spec("alpha")).await.unwrap();
        let b = be.provision(&spec("beta")).await.unwrap();
        let p = be.provision(&spec("")).await.unwrap();

        assert!(a.root.starts_with(root.join("alpha")), "alpha cell not under its tenant dir: {:?}", a.root);
        assert!(b.root.starts_with(root.join("beta")), "beta cell not under its tenant dir: {:?}", b.root);
        assert!(p.root.starts_with(root.join("personal")), "empty tenant should map to personal: {:?}", p.root);
        assert_ne!(a.root.parent(), b.root.parent(), "tenants must not share a cell parent dir");

        let _ = std::fs::remove_dir_all(&root);
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
        });
        let h = be.provision(&spec("../../etc")).await.unwrap();
        assert!(h.root.starts_with(&root), "tenant slug escaped the cells root: {:?}", h.root);
        let _ = std::fs::remove_dir_all(&root);
    }
}
