//! Litebox running NATIVELY on macOS (Apple Silicon): the unmodified Linux-ABI ELF runs with its
//! `SVC` instructions trapped and emulated against `litebox_shim_linux` (no AOT rewrite, no guest
//! kernel). Opening `utun` needs root, so the runner runs under `sudo -n` for its whole life
//! (`/etc/sudoers.d/hive-litebox-macos` scopes it to the runner path + `ifconfig utunN`) and the
//! tenant boundary is its Seatbelt profile, installed before any guest code runs. Per-cell `/30`s
//! ride CLI flags (`sudo` `env_reset` strips env vars); a `utun` exists only while the runner holds
//! it, so [`start`] spawns first, then addresses it, and killing the runner is the whole teardown.
//! Capability is probed ([`available`] does a real `sudo -n` round trip), never assumed. Scope is
//! `start_function` only — builds stay on the bare host, so native addons fail to `dlopen` here.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Config knobs.
#[derive(Clone, Debug)]
pub struct LiteboxMacosConfig {
    /// Path to the prebuilt `litebox_runner_linux_on_macos_userland` binary
    /// (`HIVE_LITEBOX_MACOS_RUNNER_BIN`, else a stable per-operator install path).
    pub runner_bin: PathBuf,
}

impl Default for LiteboxMacosConfig {
    fn default() -> Self {
        let runner_bin = std::env::var("HIVE_LITEBOX_MACOS_RUNNER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/usr/local/bin/litebox-runner-macos"));
        LiteboxMacosConfig { runner_bin }
    }
}

/// Tier 1: cheap existence probe — mirrors `LiteboxBackend::is_supported`'s
/// shape. NOT proof the sudo grant actually works; see [`available`].
pub fn is_supported(cfg: &LiteboxMacosConfig) -> bool {
    cfg!(target_os = "macos") && cfg.runner_bin.exists()
}

/// Tier 2: a real round trip through `sudo -n` proves the sudoers grant is live, not just that the
/// binary exists; `sudo -n` fails immediately without a prompt when it is missing.
pub async fn available(cfg: &LiteboxMacosConfig) -> bool {
    if !is_supported(cfg) {
        return false;
    }
    Command::new("sudo")
        .arg("-n")
        .arg(&cfg.runner_bin)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Is this function's runtime one the litebox-macos rootfs can serve? Node only today (the rootfs
/// bundle is Node+npm).
pub fn eligible(runtime: hive_core::Runtime) -> bool {
    runtime == hive_core::Runtime::Node
}

/// One cell's private point-to-point network identity.
#[derive(Clone, Debug)]
pub struct MacosNet {
    pub tun_dev: String,
    pub host_ip: String,
    pub guest_ip: String,
}

struct RunnerRollback {
    cfg: LiteboxMacosConfig,
    net: MacosNet,
    armed: bool,
}

impl RunnerRollback {
    fn new(cfg: LiteboxMacosConfig, net: MacosNet) -> Self {
        Self {
            cfg,
            net,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for RunnerRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let cfg = self.cfg.clone();
        let net = self.net.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                kill_runner(&cfg, &net).await;
            });
        }
    }
}

/// Allocates a fresh `/30` per cell from `10.90.x.x` (distinct from the Linux backend's
/// `10.88.x.x`); `utun` unit numbers start at 50, clear of what VPN clients hold.
pub struct NetAllocator {
    idx: AtomicU32,
}

impl Default for NetAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl NetAllocator {
    pub fn new() -> Self {
        NetAllocator {
            idx: AtomicU32::new(0),
        }
    }

    pub fn next(&self) -> MacosNet {
        let i = self.idx.fetch_add(1, Ordering::SeqCst) % 16384;
        let third = ((i >> 6) & 0xff) as u8;
        let base = ((i & 0x3f) as u8) * 4;
        MacosNet {
            tun_dev: format!("utun{}", 50 + (i % 900)),
            host_ip: format!("10.90.{third}.{}", base + 1),
            guest_ip: format!("10.90.{third}.{}", base + 2),
        }
    }
}

/// Spawn the runner against `tar_path` (base rootfs + the deployment's files, guest cwd = root),
/// then address the resulting `utun`. `env` is the function's own env plus `PORT`, nothing from
/// this host process. Returns the running child and the net identity to dial.
pub async fn start(
    cfg: &LiteboxMacosConfig,
    net: &MacosNet,
    tar_path: &std::path::Path,
    program: &str,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<tokio::process::Child> {
    let mut cmd = Command::new("sudo");
    cmd.arg("-n")
        .arg(&cfg.runner_bin)
        .arg("-Z")
        .arg(format!("--tun-device-name={}", net.tun_dev))
        .arg(format!("--guest-ip={}", net.guest_ip))
        .arg(format!("--gateway-ip={}", net.host_ip))
        .arg(format!("--initial-files={}", tar_path.display()));
    for (k, v) in env {
        cmd.arg("--env").arg(format!("{k}={v}"));
    }
    cmd.arg(program);
    for a in args {
        cmd.arg(a);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn litebox-macos runner via sudo: {e}"))?;
    let mut rollback = RunnerRollback::new(cfg.clone(), net.clone());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let exists = Command::new("ifconfig")
            .arg(&net.tun_dev)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if exists {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "litebox-macos: {} never appeared — the runner may have failed before opening \
                 its utun device (check sudoers grant + runner binary)",
                net.tun_dev
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let assign = Command::new("sudo")
        .arg("-n")
        .arg("/sbin/ifconfig")
        .arg(&net.tun_dev)
        .arg(&net.host_ip)
        .arg(&net.guest_ip)
        .arg("netmask")
        .arg("255.255.255.252")
        .arg("up")
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    anyhow::ensure!(
        assign,
        "litebox-macos: failed to address {} ({} <-> {}) — is the ifconfig sudoers rule \
         installed?",
        net.tun_dev,
        net.host_ip,
        net.guest_ip
    );

    rollback.commit();
    Ok(child)
}

/// Build a combined `--initial-files` tar at `dest`: `base_rootfs` plus `workdir`'s tree at paths
/// relative to its root, appended via `tar -rf` (round-tripping the base through extract+retar
/// corrupts its packaged symlinks under BSD tar).
pub async fn build_combined_tar(
    base_rootfs: &std::path::Path,
    workdir: &std::path::Path,
    dest: &std::path::Path,
) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::copy(base_rootfs, dest).await.map_err(|e| {
        anyhow::anyhow!(
            "failed to copy base rootfs {} -> {}: {e}",
            base_rootfs.display(),
            dest.display()
        )
    })?;

    let find_out = Command::new("find")
        .arg(workdir)
        .arg("-mindepth")
        .arg("1")
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run find: {e}"))?;
    anyhow::ensure!(
        find_out.status.success(),
        "find failed listing {}: {}",
        workdir.display(),
        String::from_utf8_lossy(&find_out.stderr)
    );
    let prefix = format!("{}/", workdir.display());
    let rel_list: String = String::from_utf8_lossy(&find_out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    if rel_list.is_empty() {
        return Ok(());
    }

    let mut child = Command::new("tar")
        .arg("-C")
        .arg(workdir)
        .arg("-T")
        .arg("-")
        .arg("-rf")
        .arg(dest)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn tar: {e}"))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(rel_list.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("failed to write file list to tar: {e}"))?;
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| anyhow::anyhow!("failed waiting on tar: {e}"))?;
    anyhow::ensure!(
        out.status.success(),
        "tar append of {} into {} failed: {}",
        workdir.display(),
        dest.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// Best-effort teardown: kill the runner's OWN process (not just the `sudo` wrapper, which forks),
/// matched by its binary basename AND the cell's unique `--tun-device-name=` argument.
pub async fn kill_runner(cfg: &LiteboxMacosConfig, net: &MacosNet) {
    let basename = cfg
        .runner_bin
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("litebox_runner_linux_on_macos_userland");
    let _ = Command::new("sudo")
        .arg("-n")
        .arg("/usr/bin/pkill")
        .arg("-9")
        .arg("-f")
        .arg(format!("{basename}.*--tun-device-name={}", net.tun_dev))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}
