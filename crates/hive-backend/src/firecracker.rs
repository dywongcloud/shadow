//! Firecracker cell backend — a cell is a real aarch64 microVM on KVM.
//!
//! Runs inside a Lima VM started with `nestedVirtualization=true` on an Apple
//! Silicon M3/M4 host, where `/dev/kvm` is exposed to the guest. This is the
//! faithful analogue of Hive's design: one cell == one Firecracker process,
//! and the box daemon (this code) talks to the cell daemon (`hive-cell-agent`,
//! running inside the guest) over **vsock**.
//!
//! Lifecycle here mirrors the control plane's expectations:
//! * [`provision`](FirecrackerBackend::provision) boots a microVM and leaves the
//!   in-guest agent idle — this is what the warm pool pre-pays.
//! * [`run_build`](FirecrackerBackend::run_build) connects to the agent over
//!   vsock, ships the job, and streams logs/result back.
//! * [`terminate`](FirecrackerBackend::terminate) kills the Firecracker process
//!   and reclaims the cell's runtime dir (cells are single-use).
//!
//! It compiles on all platforms; it only *works* where `/dev/kvm` and the
//! `firecracker` binary exist.

use crate::{CellBackend, CellEndpoint, CellHandle, CellSpec, FunctionLaunch, LogSink};
use async_trait::async_trait;
use hive_core::{
    now_ms, AgentEvent, AgentRequest, BuildJob, BuildResult, CellId, LogLine, LogStream,
    CELL_AGENT_PORT, CELL_FUNCTION_PORT, CELL_GUEST_CID,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub struct FirecrackerConfig {
    pub firecracker_bin: PathBuf,
    /// Uncompressed guest kernel (vmlinux) image.
    pub kernel_image: PathBuf,
    /// Directory of per-image base rootfs files, named `<image>.ext4`.
    /// Image names with `/` or `:` are sanitized to `_`.
    pub rootfs_dir: PathBuf,
    /// Per-cell runtime state (api sockets, overlay disks, vsock) lives here.
    pub run_dir: PathBuf,
    /// Logical name of the shared base rootfs to boot when an image has no
    /// dedicated `<image>.ext4` (the common case: every deployment boots the same
    /// runtime base and gets its build output from the attached data drive).
    pub base_image: String,
    /// Host-side build cache directory (tarballs keyed by cache key). Survives
    /// the ephemeral microVMs — the cross-build cache the agent restores from.
    pub cache_dir: PathBuf,
    pub boot_args: String,
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        FirecrackerConfig {
            firecracker_bin: PathBuf::from("/usr/local/bin/firecracker"),
            kernel_image: PathBuf::from("/var/lib/hive/vmlinux"),
            rootfs_dir: PathBuf::from("/var/lib/hive/rootfs"),
            run_dir: PathBuf::from("/var/lib/hive/run"),
            base_image: "default".to_string(),
            cache_dir: PathBuf::from("/var/lib/hive/cache"),
            // The agent runs as PID1 (init=). The rootfs drive is the first
            // virtio-blk device, so the kernel finds it at /dev/vda.
            boot_args:
                "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/sbin/hive-cell-agent"
                    .to_string(),
        }
    }
}

pub struct FirecrackerBackend {
    cfg: FirecrackerConfig,
    /// Live Firecracker processes, keyed by cell. Kept here because `CellHandle`
    /// is `Clone` and stored in the control plane, but a `Child` is not.
    procs: Arc<Mutex<HashMap<CellId, Child>>>,
}

impl FirecrackerBackend {
    pub fn new(cfg: FirecrackerConfig) -> Self {
        FirecrackerBackend {
            cfg,
            procs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Probe so the box daemon can choose mock vs. firecracker.
    pub fn is_supported(&self) -> bool {
        cfg!(target_os = "linux")
            && std::path::Path::new("/dev/kvm").exists()
            && self.cfg.firecracker_bin.exists()
    }

    fn rootfs_for(&self, image: &str) -> PathBuf {
        self.cfg.rootfs_dir.join(format!("{}.ext4", sanitize_image(image)))
    }

    /// Per-deployment build-output ext4 (the artifact `deliver_build` packs and
    /// `provision` attaches as the cell's second drive). Lives alongside the
    /// base rootfs images, keyed by the same logical image name.
    fn data_image_for(&self, image: &str) -> PathBuf {
        self.cfg.rootfs_dir.join(format!("{}.data.ext4", sanitize_image(image)))
    }
}

/// Guest path the per-deployment build output is mounted at (the agent mounts
/// `/dev/vdb` here; the function server runs with this as its working dir).
pub const DELIVERED_WORKDIR: &str = "/build";

fn sanitize_image(image: &str) -> String {
    image
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect()
}

#[async_trait]
impl CellBackend for FirecrackerBackend {
    fn name(&self) -> &'static str {
        "firecracker"
    }

    async fn provision(&self, spec: &CellSpec) -> anyhow::Result<CellHandle> {
        anyhow::ensure!(
            self.is_supported(),
            "firecracker backend unavailable: need Linux + /dev/kvm + {} (run inside the Lima VM)",
            self.cfg.firecracker_bin.display()
        );

        // Per-tenant run dir (`<run_dir>/<tenant>/<cell-id>`) so each team's VM
        // sockets / overlays / console logs are isolated on the host. (The VM
        // itself is already isolated by its own kernel + per-cell vsock; this
        // partitions the host-side artifacts too.) Empty tenant => "personal".
        let tenant = if spec.tenant.trim().is_empty() { "personal" } else { spec.tenant.as_str() };
        let run_dir = self.cfg.run_dir.join(crate::sanitize_tenant(tenant)).join(spec.id.as_str());
        tokio::fs::create_dir_all(&run_dir).await?;

        let api_sock = run_dir.join("api.sock");
        let vsock_uds = run_dir.join("vsock.sock");
        let log_file = run_dir.join("console.log");
        let overlay = run_dir.join("rootfs.ext4");

        // Per-cell writable rootfs. Real Hive uses CoW; a copy is the simple,
        // correct equivalent for a study implementation. Prefer a dedicated
        // `<image>.ext4` but fall back to the shared base runtime rootfs — most
        // deployments boot the base and get their code from the data drive below.
        let base = {
            let per_image = self.rootfs_for(&spec.image);
            if per_image.exists() { per_image } else { self.rootfs_for(&self.cfg.base_image) }
        };
        anyhow::ensure!(
            base.exists(),
            "rootfs missing for image '{}' and base '{}': {}",
            spec.image,
            self.cfg.base_image,
            base.display()
        );
        tokio::fs::copy(&base, &overlay).await?;

        // If this image has a delivered build artifact (packed by `deliver_build`),
        // give the cell a private writable copy to attach as its second drive.
        // The in-guest agent mounts it at DELIVERED_WORKDIR (/dev/vdb -> /build).
        let data_src = self.data_image_for(&spec.image);
        let data_overlay = run_dir.join("data.ext4");
        let has_data = data_src.exists();
        if has_data {
            tokio::fs::copy(&data_src, &data_overlay).await?;
        }

        // Spawn the Firecracker process bound to a fresh API socket.
        let _ = tokio::fs::remove_file(&api_sock).await;
        let console = std::fs::File::create(&log_file)?;
        let child = Command::new(&self.cfg.firecracker_bin)
            .arg("--api-sock")
            .arg(&api_sock)
            .arg("--id")
            .arg(spec.id.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::from(console.try_clone()?))
            .stderr(Stdio::from(console))
            .kill_on_drop(true)
            .spawn()?;
        self.procs.lock().await.insert(spec.id.clone(), child);

        // Wait for the API socket to appear.
        wait_for_path(&api_sock, Duration::from_secs(5)).await?;

        // Configure the microVM via the REST API, then boot it.
        let mem_mib = spec.resources.mem_mib;
        let vcpus = spec.resources.vcpus;
        fc_put(
            &api_sock,
            "/machine-config",
            &serde_json::json!({ "vcpu_count": vcpus, "mem_size_mib": mem_mib, "smt": false }),
        )
        .await?;
        fc_put(
            &api_sock,
            "/boot-source",
            &serde_json::json!({
                "kernel_image_path": self.cfg.kernel_image,
                "boot_args": self.cfg.boot_args,
            }),
        )
        .await?;
        fc_put(
            &api_sock,
            "/drives/rootfs",
            &serde_json::json!({
                "drive_id": "rootfs",
                "path_on_host": overlay,
                "is_root_device": true,
                "is_read_only": false,
            }),
        )
        .await?;
        // Second drive: the delivered build output (/dev/vdb in the guest).
        if has_data {
            fc_put(
                &api_sock,
                "/drives/data",
                &serde_json::json!({
                    "drive_id": "data",
                    "path_on_host": data_overlay,
                    "is_root_device": false,
                    "is_read_only": false,
                }),
            )
            .await?;
        }
        // vsock device: the host endpoint is `vsock_uds`; the guest agent
        // listens on CELL_AGENT_PORT and we host-initiate a CONNECT later.
        fc_put(
            &api_sock,
            "/vsock",
            &serde_json::json!({
                "guest_cid": CELL_GUEST_CID,
                "uds_path": vsock_uds,
            }),
        )
        .await?;
        // virtio-rng entropy device. Without a hardware RNG the guest's entropy
        // pool starts empty, so anything that calls getrandom() at startup (e.g.
        // Node's crypto init) BLOCKS for many seconds until the pool fills — long
        // enough that the function misses its readiness window. Feeding the guest
        // entropy makes cold starts fast and deterministic. Best-effort: older
        // Firecracker without the device simply 400s and we proceed.
        let _ = fc_put(&api_sock, "/entropy", &serde_json::json!({})).await;
        fc_put(
            &api_sock,
            "/actions",
            &serde_json::json!({ "action_type": "InstanceStart" }),
        )
        .await?;

        Ok(CellHandle {
            id: spec.id.clone(),
            image: spec.image.clone(),
            resources: spec.resources.clone(),
            root: run_dir,
            endpoint: Some(vsock_uds.to_string_lossy().into_owned()),
        })
    }

    async fn run_build(
        &self,
        cell: &CellHandle,
        job: &BuildJob,
        sink: LogSink,
    ) -> anyhow::Result<BuildResult> {
        let uds = cell
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cell {} has no vsock endpoint", cell.id))?;

        // The agent may still be booting; retry the host-initiated CONNECT.
        let mut stream = connect_agent(uds, Duration::from_secs(20)).await?;

        let _ = sink.send(LogLine {
            ts_ms: now_ms(),
            stream: LogStream::System,
            line: format!("[{}] connected to cell agent; dispatching build", cell.id),
        });

        // Send the job.
        let req = serde_json::to_vec(&AgentRequest::Run(job.clone()))?;
        write_frame(&mut stream, &req).await?;

        let started_at_ms = now_ms();
        let timeout = Duration::from_secs(job.resources.timeout_secs.max(1));

        let cache_dir = self.cfg.cache_dir.clone();
        let pump = async {
            loop {
                let frame = read_frame(&mut stream).await?;
                let ev: AgentEvent = serde_json::from_slice(&frame)?;
                match ev {
                    AgentEvent::Log(line) => {
                        let _ = sink.send(line);
                    }
                    AgentEvent::Done(res) => return anyhow::Ok(res),
                    // Build cache: serve the restore tarball from host disk.
                    AgentEvent::CacheGet { key, .. } => {
                        let tar = tokio::fs::read(cache_path(&cache_dir, &key))
                            .await
                            .unwrap_or_default();
                        let _ = sink.send(LogLine {
                            ts_ms: now_ms(),
                            stream: LogStream::System,
                            line: format!(
                                "[{}] cache {} [{key}] ({} bytes)",
                                cell.id,
                                if tar.is_empty() { "miss" } else { "hit" },
                                tar.len()
                            ),
                        });
                        let reply = serde_json::to_vec(&AgentRequest::CacheData { tar })?;
                        write_frame(&mut stream, &reply).await?;
                    }
                    // Build cache: persist the tarball produced by the build.
                    AgentEvent::CachePut { key, tar } => {
                        let _ = tokio::fs::create_dir_all(&cache_dir).await;
                        let _ = tokio::fs::write(cache_path(&cache_dir, &key), &tar).await;
                    }
                    AgentEvent::Pong
                    | AgentEvent::FunctionReady
                    | AgentEvent::FunctionError(_) => {}
                }
            }
        };

        match tokio::time::timeout(timeout, pump).await {
            Ok(res) => res,
            Err(_) => {
                let _ = sink.send(LogLine {
                    ts_ms: now_ms(),
                    stream: LogStream::System,
                    line: format!("[{}] build exceeded {timeout:?}; killing cell", cell.id),
                });
                Ok(BuildResult {
                    job_id: job.id.clone(),
                    exit_code: -1,
                    timed_out: true,
                    started_at_ms,
                    finished_at_ms: now_ms(),
                })
            }
        }
    }

    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint> {
        let uds = cell
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cell {} has no vsock endpoint", cell.id))?;

        // The control plane registers the pool's workdir as its own host build
        // dir (correct for the same-host mock backend). Inside the guest that
        // path doesn't exist; the delivered build is mounted at DELIVERED_WORKDIR.
        let mut func = func.clone();
        func.workdir = Some(DELIVERED_WORKDIR.to_string());

        let mut stream = connect_agent(uds, Duration::from_secs(20)).await?;
        let req = serde_json::to_vec(&AgentRequest::StartFunction(func.clone()))?;
        write_frame(&mut stream, &req).await?;

        // Wait for the agent to report the function is up and bridged.
        loop {
            let frame = read_frame(&mut stream).await?;
            match serde_json::from_slice::<AgentEvent>(&frame)? {
                AgentEvent::FunctionReady => break,
                AgentEvent::FunctionError(e) => anyhow::bail!("function failed to start: {e}"),
                _ => continue,
            }
        }
        Ok(CellEndpoint::Vsock {
            uds: uds.clone(),
            port: CELL_FUNCTION_PORT,
        })
    }

    fn delivered_workdir(&self) -> Option<&'static str> {
        Some(DELIVERED_WORKDIR)
    }

    async fn deliver_build(&self, image: &str, build_dir: &std::path::Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            build_dir.is_dir(),
            "deliver_build: build dir does not exist: {}",
            build_dir.display()
        );
        tokio::fs::create_dir_all(&self.cfg.rootfs_dir).await?;
        let out = self.data_image_for(image);
        let tmp = out.with_extension("ext4.tmp");

        // Pack the host build dir into an ext4 image WITHOUT a privileged loop
        // mount: `mkfs.ext4 -d <dir>` populates the filesystem from a directory
        // directly. Size it to the build dir plus generous headroom (node_modules
        // etc.). Written to a temp path then atomically renamed so a serving cell
        // never attaches a half-written image.
        let script = format!(
            r#"set -e
            sz=$(du -sm "$BUILD" | cut -f1)
            sz=$(( sz * 3 / 2 + 512 ))
            rm -f "$OUT"
            truncate -s "${{sz}}M" "$OUT"
            mkfs.ext4 -F -q -d "$BUILD" "$OUT"
            "#,
        );
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env("BUILD", build_dir)
            .env("OUT", &tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        anyhow::ensure!(status.success(), "mkfs.ext4 -d failed packing build output for image '{image}'");
        tokio::fs::rename(&tmp, &out).await?;
        Ok(())
    }

    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()> {
        if let Some(mut child) = self.procs.lock().await.remove(&cell.id) {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        let _ = tokio::fs::remove_dir_all(&cell.root).await;
        Ok(())
    }
}

// ---- Firecracker REST API over its Unix socket ------------------------------

/// Minimal HTTP/1.1 `PUT` to the Firecracker API socket. Firecracker speaks
/// plain HTTP over a Unix domain socket; we avoid a full HTTP client dep and
/// write the request by hand, expecting a 2xx (usually 204).
async fn fc_put(api_sock: &PathBuf, path: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    let body = serde_json::to_vec(body)?;
    let mut stream = UnixStream::connect(api_sock).await?;
    let req = format!(
        "PUT {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;

    // Firecracker's micro-http server keeps the connection open (it ignores our
    // `Connection: close`), so we must NOT read to EOF — parse headers, then
    // read exactly Content-Length bytes of body.
    let resp = read_http_response(&mut stream).await?;
    let head = String::from_utf8_lossy(&resp);
    let status_line = head.lines().next().unwrap_or("<no status>");
    let status_ok = status_line.contains(" 200")
        || status_line.contains(" 204")
        || status_line.contains(" 201");
    anyhow::ensure!(
        status_ok,
        "firecracker API PUT {path} failed: {status_line} | body: {}",
        head.split("\r\n\r\n").nth(1).unwrap_or("")
    );
    Ok(())
}

/// Read one HTTP response (status line + headers + Content-Length body) without
/// waiting for the connection to close.
async fn read_http_response(stream: &mut UnixStream) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        // Find end of headers.
        if let Some(hdr_end) = find_subslice(&buf, b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&buf[..hdr_end]).to_ascii_lowercase();
            let content_len = header
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = hdr_end + 4;
            if buf.len() >= body_start + content_len {
                return Ok(buf);
            }
        }
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut tmp))
            .await
            .map_err(|_| anyhow::anyhow!("timed out reading firecracker API response"))??;
        anyhow::ensure!(n > 0, "firecracker API closed connection mid-response");
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Host path for a build-cache key's tarball (key sanitized for the filesystem).
fn cache_path(cache_dir: &PathBuf, key: &str) -> PathBuf {
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    cache_dir.join(format!("{safe}.tar.gz"))
}

async fn wait_for_path(path: &PathBuf, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("timed out waiting for {}", path.display())
}

// ---- vsock host-initiated connection + framing ------------------------------

/// Host-initiated vsock connect: connect to the Firecracker vsock UDS, send the
/// `CONNECT <port>` handshake, and wait for `OK <peer_port>`. Retries until the
/// in-guest agent is listening (i.e. the cell has finished booting).
async fn connect_agent(uds: &str, timeout: Duration) -> anyhow::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_err = String::from("unknown");
    while tokio::time::Instant::now() < deadline {
        match try_connect_once(uds).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err = e.to_string();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    anyhow::bail!("could not reach cell agent on {uds}: {last_err}")
}

async fn try_connect_once(uds: &str) -> anyhow::Result<UnixStream> {
    let mut stream = UnixStream::connect(uds).await?;
    stream
        .write_all(format!("CONNECT {CELL_AGENT_PORT}\n").as_bytes())
        .await?;
    stream.flush().await?;

    // Read the "OK <port>\n" line one byte at a time (it's short).
    let mut reader = BufReader::new(&mut stream);
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        anyhow::ensure!(n == 1, "vsock handshake closed early");
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        anyhow::ensure!(line.len() < 64, "vsock handshake line too long");
    }
    let line = String::from_utf8_lossy(&line);
    anyhow::ensure!(line.starts_with("OK"), "vsock handshake rejected: {line}");
    Ok(stream)
}

/// Length-prefixed framing: 4-byte big-endian length, then JSON payload.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, payload: &[u8]) -> anyhow::Result<()> {
    let len = (payload.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    anyhow::ensure!(n <= 64 * 1024 * 1024, "frame too large: {n}");
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}
