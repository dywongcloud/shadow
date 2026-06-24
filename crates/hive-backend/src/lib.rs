//! `hive-backend` — the pluggable isolation layer.
//!
//! In real Hive a **cell** is a Firecracker microVM on KVM. The control plane,
//! warm pool, scheduler, and lifecycle logic do not care *how* a cell is
//! realized — they only need to provision one, run a build in it, and tear it
//! down. That contract is [`CellBackend`].
//!
//! Two implementations ship here:
//!
//! * [`mock::MockBackend`] — a cell is a sandboxed child-process build in a temp
//!   dir. Runs on macOS/M-series today, so the whole control plane is testable
//!   without virtualization.
//! * [`firecracker::FirecrackerBackend`] — a cell is a real aarch64 Firecracker
//!   microVM, intended to run inside a Lima VM with nested virtualization.

pub mod firecracker;
pub mod mock;

use async_trait::async_trait;
use hive_core::{BuildJob, BuildResult, CellId, LogLine, ResourceSpec};
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub use hive_core::FunctionLaunch;

/// Sink the backend writes build output to. The box daemon fans this out to
/// any number of log subscribers.
pub type LogSink = mpsc::UnboundedSender<LogLine>;

/// An address the gateway can open connections to in order to reach a function
/// server running inside a cell (Fluid compute data plane).
#[derive(Clone, Debug)]
pub enum CellEndpoint {
    /// Direct TCP (mock backend): the function listens here on the host.
    Tcp(String),
    /// Firecracker: host-initiated vsock CONNECT to `port` on `uds`.
    Vsock { uds: String, port: u32 },
}

/// A bidirectional byte stream to a function instance.
pub trait DuplexStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> DuplexStream for T {}

/// Open a fresh connection to a cell endpoint. One connection per in-flight
/// request keeps proxying simple; in-function concurrency = many of these open
/// to the same instance at once.
pub async fn connect_endpoint(ep: &CellEndpoint) -> anyhow::Result<Box<dyn DuplexStream>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    match ep {
        CellEndpoint::Tcp(addr) => Ok(Box::new(tokio::net::TcpStream::connect(addr).await?)),
        CellEndpoint::Vsock { uds, port } => {
            let mut s = tokio::net::UnixStream::connect(uds).await?;
            s.write_all(format!("CONNECT {port}\n").as_bytes()).await?;
            s.flush().await?;
            // Consume the "OK <peer_port>\n" handshake line.
            let mut line = Vec::new();
            let mut b = [0u8; 1];
            loop {
                let n = s.read(&mut b).await?;
                anyhow::ensure!(n == 1, "vsock handshake closed early");
                if b[0] == b'\n' {
                    break;
                }
                line.push(b[0]);
                anyhow::ensure!(line.len() < 64, "vsock handshake too long");
            }
            anyhow::ensure!(
                line.starts_with(b"OK"),
                "vsock handshake rejected: {}",
                String::from_utf8_lossy(&line)
            );
            Ok(Box::new(s))
        }
    }
}

/// A container (podman) cell: instead of a microVM / host process, the backend
/// runs this OCI image directly via podman on the HOST. Set when the deployment's
/// function is `runtime == "container"`. Lets Firecracker nodes run containers on
/// the host (outside the microVM) — not just the mock backend.
#[derive(Clone, Debug)]
pub struct ContainerSpec {
    /// OCI image to `podman run` (from the function's `__container__` start_cmd).
    pub image: String,
    /// Port the container listens on inside (host port is allocated + mapped).
    pub port: u16,
}

/// Run an OCI image as a detached podman container on the HOST, then front it with
/// the Fluid tunnel server (so in-function concurrency + the gateway proxy work the
/// same as any other cell). Shared by the mock + Firecracker backends so Firecracker
/// nodes can run containers outside their microVMs. Returns the podman container
/// name (for teardown), the tunnel endpoint, and the accept-loop task handle.
pub(crate) async fn podman_run_container(
    cell_id: &CellId,
    image: &str,
    internal_port: u16,
    host_port: u16,
    env: &std::collections::BTreeMap<String, String>,
    max_concurrency: u32,
    path_env: &str,
    // Optional OCI runtime (e.g. gVisor's `runsc` for stronger sandboxing). `None`
    // uses podman's default (crun/runc). A name or absolute path podman accepts.
    runtime: Option<&str>,
) -> anyhow::Result<(String, CellEndpoint, tokio::task::JoinHandle<()>)> {
    use tokio::process::Command;
    let name = format!("hive-{}", cell_id.as_str().replace(|c: char| !c.is_ascii_alphanumeric(), "-"));
    // Clear any stale container from a prior cell at this id (kill_on_drop can't).
    let _ = Command::new("podman").args(["rm", "-f", &name]).env("PATH", path_env).output().await;
    // Base `podman run` args (everything after the subcommand, sans --runtime). The
    // port is published on 127.0.0.1 ONLY — the container is never exposed to the
    // internet directly; it's reached solely via the gateway/ngrok for the deployment.
    let mut base: Vec<String> = vec![
        "-d".into(), "--name".into(), name.clone(),
        "-e".into(), format!("PORT={internal_port}"),
    ];
    for (k, v) in env {
        base.push("-e".into());
        base.push(format!("{k}={v}"));
    }
    base.push("-p".into());
    base.push(format!("127.0.0.1:{host_port}:{internal_port}"));
    base.push(image.to_string());

    // Attempt 1: with the requested sandbox runtime (e.g. gVisor `runsc`) if any.
    let mut attempt: Vec<String> = vec!["run".into()];
    if let Some(rt) = runtime {
        attempt.push("--runtime".into());
        attempt.push(rt.to_string());
    }
    attempt.extend(base.iter().cloned());
    let mut out = Command::new("podman").args(&attempt).env("PATH", path_env).output().await?;

    // Non-breaking fallback: if a sandbox runtime was requested but the container
    // couldn't start under it (a gVisor incompatibility), retry on podman's DEFAULT
    // runtime so the deployment still serves. Isolation degrades to the default for
    // that one container; logged so the degradation is visible.
    if !out.status.success() && runtime.is_some() {
        tracing::warn!(
            runtime = ?runtime,
            err = %String::from_utf8_lossy(&out.stderr).trim(),
            "container failed under sandbox runtime — retrying with podman default runtime"
        );
        let _ = Command::new("podman").args(["rm", "-f", &name]).env("PATH", path_env).output().await;
        let mut fb: Vec<String> = vec!["run".into()];
        fb.extend(base.iter().cloned());
        out = Command::new("podman").args(&fb).env("PATH", path_env).output().await?;
    }
    anyhow::ensure!(
        out.status.success(),
        "podman run failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    // Wait for the container's port to accept connections (image pull + boot).
    let func_addr = format!("127.0.0.1:{host_port}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if tokio::net::TcpStream::connect(&func_addr).await.is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = Command::new("podman").args(["rm", "-f", &name]).env("PATH", path_env).output().await;
            anyhow::bail!("container {name} not ready on {func_addr} within 90s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let tunnel_addr = listener.local_addr()?.to_string();
    let max_conc = max_concurrency.max(1);
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((conn, _)) => {
                    let local = func_addr.clone();
                    tokio::spawn(async move {
                        fluid_tunnel::TunnelServer::serve(conn, local, max_conc).await;
                    });
                }
                Err(_) => break,
            }
        }
    });
    Ok((name, CellEndpoint::Tcp(tunnel_addr), task))
}

/// Stop + remove a podman container by name (cell teardown). Best-effort.
pub(crate) async fn podman_stop_container(name: &str, path_env: &str) {
    let _ = tokio::process::Command::new("podman")
        .args(["rm", "-f", name])
        .env("PATH", path_env)
        .output()
        .await;
}

/// What the control plane asks a backend to materialize.
#[derive(Clone, Debug)]
pub struct CellSpec {
    pub id: CellId,
    /// Logical image / rootfs name (warm pools are keyed on this).
    pub image: String,
    pub resources: ResourceSpec,
    /// Owning team/tenant slug (empty = "personal"). Lets a backend partition a
    /// cell's host resources per tenant (the mock backend nests cell workdirs
    /// under the tenant; Firecracker nests its per-cell run dir likewise).
    pub tenant: String,
    /// Set when this cell is a CONTAINER (podman) rather than a function/microVM.
    /// The backend runs it via host podman; Firecracker uses this to skip booting a
    /// microVM and run the container on the host instead. `None` = function cell.
    pub container: Option<ContainerSpec>,
}

/// Make a tenant slug safe to use as a single host path component (no traversal,
/// no separators) so an odd/hostile team name can't escape a backend's cells
/// root. Empty / dot-only normalizes to "personal".
pub(crate) fn sanitize_tenant(t: &str) -> String {
    let s: String = t
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') { "personal".into() } else { s }
}

/// A live, backend-specific handle to a provisioned cell.
#[derive(Clone, Debug)]
pub struct CellHandle {
    pub id: CellId,
    pub image: String,
    pub resources: ResourceSpec,
    /// Filesystem root the build runs against (temp dir for mock, mount for FC).
    pub root: PathBuf,
    /// Opaque backend handle, e.g. the Firecracker API socket path or VM id.
    pub endpoint: Option<String>,
}

/// The isolation contract. One impl == one way of realizing a cell.
#[async_trait]
pub trait CellBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Boot/prepare a cell. This is the expensive step Hive hides behind warm
    /// pools — for the real backend it boots a microVM and loads the image.
    async fn provision(&self, spec: &CellSpec) -> anyhow::Result<CellHandle>;

    /// Run a build inside an already-provisioned cell, streaming logs to `sink`.
    async fn run_build(
        &self,
        cell: &CellHandle,
        job: &BuildJob,
        sink: LogSink,
    ) -> anyhow::Result<BuildResult>;

    /// Make a built deployment available to the cells that will serve it.
    ///
    /// The control plane builds on its own host filesystem (`build_dir`), then
    /// serves via `provision` + `start_function`. For a same-host backend (mock,
    /// child process) the serving cell already sees `build_dir`, so this is a
    /// no-op. For an isolated backend (Firecracker microVM) the guest cannot see
    /// the host's `build_dir`, so this packs it into a per-`image` artifact that
    /// `provision` later attaches to the cell. Called once per deployment, keyed
    /// by the same `image` the function pool will provision with.
    async fn deliver_build(&self, _image: &str, _build_dir: &std::path::Path) -> anyhow::Result<()> {
        Ok(())
    }

    /// The guest path a delivered build is mounted at inside a cell (so the
    /// control plane can register the function pool's workdir to match). For
    /// same-host backends the workdir is the host `build_dir` (returns `None`,
    /// meaning "use the build dir as-is"); Firecracker mounts it at a fixed
    /// guest path.
    fn delivered_workdir(&self) -> Option<&'static str> {
        None
    }

    /// Start a long-lived function server inside the cell and return an endpoint
    /// the gateway can open connections to (Fluid compute serving path). Unlike
    /// `run_build`, the cell is NOT single-use: it stays alive serving many
    /// concurrent requests until the pool decides to scale it down.
    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint>;

    /// Tear the cell down. Cells are single-use for builds; for functions this
    /// is called when the instance is scaled down.
    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()>;
}
