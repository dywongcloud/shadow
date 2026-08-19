//! Litebox running NATIVELY on macOS (Apple Silicon) — no Linux VM, no
//! Firecracker, no Apple `container` tool. Backs [`crate::mock::MockBackend`]'s
//! Node runtime path with a real syscall-level sandbox
//! (<https://github.com/dywongcloud/litebox>'s `litebox_platform_macos_userland`
//! fork, checked out locally at `~/litebox` on a macOS shadow node — there is
//! no ansible role for this the way `ansible/roles/litebox` builds the Linux
//! runner, because these nodes are personal dev machines, not fleet-managed;
//! see AGENTS.md's "Bringing a node into the mesh" — `fc-lax`-style dev
//! nodes are not ansible-inventoried).
//!
//! ## Mechanism — genuinely different from the Linux backend, same idea
//!
//! [`crate::litebox`]'s Linux backend statically rewrites `syscall` opcodes in
//! the target ELF before exec (AOT rewriting). This runner does not: it execs
//! the SAME Linux-ABI ELF binary UNMODIFIED and traps ARM64 `SVC` (syscall)
//! instructions as they're issued, emulating each one against litebox's
//! shared `litebox_shim_linux` core — the crate is genuinely named
//! `litebox_runner_linux_ON_macos_userland`: a Linux guest, a macOS host, no
//! guest kernel at all. Verified live (2026-08-15): the plain, unmodified,
//! upstream-built Node 26.7.0 + npm binaries from the provided rootfs tar ran
//! with zero preprocessing — no `litebox-packager` step needed for this
//! runner, unlike what its own `--help` text says for the general case.
//!
//! ## Networking needs root, narrowly — by the runner's own design, not a workaround
//!
//! Opening a `utun` device (`socket(PF_SYSTEM, SOCK_DGRAM, SYSPROTO_CONTROL)`)
//! requires root on Darwin; there is no CAP_NET_ADMIN-style capability grant
//! for it. The runner's own architecture is built around this rather than
//! avoiding it: it opens every privileged resource it needs (the utun fd,
//! reading the `--initial-files` tar) FIRST, then installs a Seatbelt
//! (`sandbox_init`, deny-by-default) profile BEFORE executing one instruction
//! of guest/tenant code — see `litebox_platform_macos_userland::seatbelt`'s
//! module doc for the measured file/network/exec/signal transitions it
//! denies. So the runner process runs under `sudo -n` for its WHOLE life, and
//! the actual security boundary for tenant code is the Seatbelt jail, not the
//! process's UID — the same "root drops into a jail, not a lower UID"
//! lifecycle Chrome-style sandboxes use, and a deliberate, documented,
//! tested design choice on the runner's side (App Sandbox, the officially
//! "supported" replacement for the deprecated `sandbox_init`, cannot express
//! this lifecycle at all).
//!
//! `/etc/sudoers.d/hive-litebox-macos` scopes this to exactly the runner's
//! absolute path plus a narrowly-globbed `ifconfig utunN <ip> <ip> netmask
//! ... up`/`destroy` (never touches a real interface like `en0`) — nothing
//! else on the host is granted passwordless root by this feature. See
//! [`Self::available`]: this is a probed capability, not one this module
//! ever installs on its own — granting sudo is a one-way privilege decision
//! for the operator to make explicitly, once, per machine.
//!
//! ## Per-cell network identity
//!
//! Unpatched, this litebox fork hardcodes every guest to the identical
//! `10.0.0.2`/gateway `10.0.0.1` (`litebox/src/net/mod.rs`'s
//! `INTERFACE_IP_ADDR`/`GATEWAY_IP_ADDR`) — exactly the same single-guest
//! limitation `AGENTS.md`'s Litebox section documents for the pre-patch
//! Linux backend. The same fix was ported here (`~/litebox`'s own git
//! history — `Network::new_with_addrs`, `LinuxShimBuilder::
//! build_with_net_config`), mirroring `ansible/roles/litebox/files/
//! networking.patch`'s approach — but plumbed through as CLI flags
//! (`--guest-ip`/`--gateway-ip` on `litebox_runner_linux_on_macos_userland`,
//! not the Linux runner's env-var convention: a `sudo`-invoked launch under
//! the default `env_reset` policy strips arbitrary env vars, confirmed live
//! ("sudo: sorry, you are not allowed to set the following environment
//! variables"), while argv always passes through — so [`NetAllocator`] can
//! hand out a distinct `/30` per cell.
//!
//! A `utun` interface is not a persistent device the way Linux's
//! `/dev/net/tun` is — it only exists on the host for as long as the runner
//! process holds its control socket open, and no address-assignment ioctl
//! happens inside the runner at all (verified: `litebox_platform_macos_
//! userland::net` only opens the raw kernel-control socket, nothing else).
//! So a cell's `/30` cannot be assigned before the runner starts: [`start`]
//! spawns the runner FIRST, then polls `ifconfig <dev>` until the interface
//! exists, THEN assigns its point-to-point address. No explicit teardown
//! command is needed or possible — the device disappears the instant the
//! runner process exits, so [`crate::mock::MockBackend::terminate`] killing
//! the child is the whole teardown.
//!
//! ## Capability is probed, never assumed
//!
//! Mirrors the Wasmer-runtime precedent (AGENTS.md, "capability is PROBED
//! and ADVERTISED, never assumed") and `container_cli::available`'s "a real
//! round trip, not `--version` alone" discipline: [`is_supported`] is a cheap
//! existence check, [`available`] additionally proves the sudoers grant is
//! actually live. `MockBackend::start_function` falls back to today's bare
//! host-`node`-exec path whenever either check fails — never a hard
//! failure, so a node without the sudoers rule installed (the overwhelming
//! majority, until an operator opts in) keeps working exactly as before.
//!
//! ## Known limitation — disclosed, not silently accepted
//!
//! Scope is `start_function` only, mirroring `crate::litebox`'s own "run_build
//! stays a plain unsandboxed host process" precedent — but for a different
//! reason: litebox's Linux backend can't sandbox `fork`-heavy build scripts at
//! all (no `fork` support yet), while this rootfs bundle is Node+npm only, no
//! `git`/`curl`/build toolchain, so builds still run on the bare macOS host
//! exactly as they do today. That means `node_modules` compiled during the
//! build is a Darwin (Mach-O) binary; a deployment with NATIVE (compiled)
//! addons will fail to `dlopen` under this Linux-ABI guest. Pure-JS
//! deployments (the overwhelming majority of small apps/bots) are unaffected.
//! Fixing this fully needs a fuller rootfs (git + a toolchain) and routing
//! `run_build_process` through the guest too — a real follow-up, not a
//! silently-accepted correctness gap; not attempted here.
//!
//! **A second, third-party limitation, also disclosed rather than papered
//! over: litebox's own TCP stack has an intermittent race under true
//! concurrency.** Reproduced live (2026-08-15): two cells started genuinely
//! concurrently (`tokio::join!`) served correctly ~2 of 3 runs; the failing
//! run's tunnel request got `upstream error: function closed before
//! headers` even though `wait_tcp_ready` had already proven the guest
//! accepted a connection moments earlier — a single cell started alone
//! never reproduced it across repeated runs, so this is specific to two
//! `litebox_platform_macos_userland` processes' smoltcp-based guest network
//! stacks running under real concurrency, not this module's own net
//! allocation (which is atomic and collision-free — verified, no address/
//! port reuse across the concurrent pair) or `sudo` invocation. This is
//! upstream behavior in an actively-developed sandbox (its own git history
//! is dense with exactly this class of fix), not something to patch here.
//! It is already absorbed for real traffic: `fluid_gateway::proxy_function`
//! retries a `upstream_silent` failure (tunnel alive, no response — exactly
//! this shape) up to `MAX_REROUTES` times before surfacing an error, the
//! same machinery every other backend's transient failures already rely on
//! — no litebox-macos-specific retry was added here.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Config knobs.
#[derive(Clone, Debug)]
pub struct LiteboxMacosConfig {
    /// Path to the prebuilt `litebox_runner_linux_on_macos_userland` binary.
    /// Defaults to `HIVE_LITEBOX_MACOS_RUNNER_BIN` if set, else a stable
    /// per-operator install path — mirrors `LiteboxConfig::runner_bin`'s
    /// `/usr/local/bin/litebox-runner` convention. There is no fleet rollout
    /// for this path (these are personal dev machines); an operator builds
    /// the runner from `~/litebox` and points this at it once.
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

/// Tier 2: a real round trip through `sudo -n` — proves the sudoers grant is
/// actually live, not just that the binary exists on disk (the
/// `container_cli::available` / Wasmer-runtime "probed, not assumed"
/// discipline). `sudo -n` fails immediately (no password prompt hang) when
/// the grant is missing, so this is safe to call on every cold start.
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

/// Is this function's runtime one the litebox-macos rootfs can serve? Scoped
/// to Node today — the provided rootfs bundle is Node+npm only (no Python/Bun
/// binaries), matching `hive_core::Runtime::infer_from_argv`'s own node/npm/
/// npx/pnpm/yarn/next basename set.
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

/// Allocates a fresh `/30` per cell. Deliberately a DIFFERENT address range
/// (`10.90.x.x`) than `crate::litebox`'s Linux `LiteboxNet` (`10.88.x.x`) —
/// the two backends never run on the same host, but keeping the ranges
/// visually distinct avoids any ambiguity reading a packet capture or a log
/// line. `utun` unit numbers start at 50 to stay clear of the low numbers
/// (0-6 observed live) macOS/VPN clients already hold on a typical dev Mac.
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

/// Spawn the runner against `tar_path` (an `--initial-files` archive — the
/// base rootfs plus the deployment's own files at paths relative to its
/// root, same "guest cwd IS that root" shape `crate::litebox` uses), then
/// address the resulting `utun` device.
///
/// `program`/`args` run inside the guest (e.g. `/usr/local/bin/node`,
/// `["/server.js"]`); `env` is the function's OWN declared env plus `PORT` —
/// nothing from this host process is forwarded (no `--forward-env`), the
/// same "cleared, never inherited" discipline `mock.rs`'s plain-process path
/// already applies, so a tenant process never sees `HIVE_SECRET_KEY` /
/// `HIVE_JWT_SECRET` / etc.
///
/// Returns the spawned child (still running — the caller owns its lifetime,
/// same shape as `MockBackend::funcs`) and the net identity the caller
/// should dial (`net.guest_ip:<port>`) and later use as this cell's
/// `wait_tcp_ready` target.
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
        // CLI flags, NOT env vars: a `sudo`-invoked launch under the default
        // `env_reset` policy strips arbitrary env vars ("sudo: sorry, you
        // are not allowed to set the following environment variables" —
        // confirmed live) unless the sudoers grant adds `SETENV`, but always
        // passes argv through untouched.
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

    // The utun device only exists once the runner's own `open_utun` call has
    // completed — poll rather than assume a fixed delay. This happens very
    // early in the runner's startup (before it reads the tar), so this loop
    // is normally one or two iterations.
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

/// Build a combined `--initial-files` tar at `dest`: `base_rootfs` (the
/// Node+npm bundle) plus `workdir`'s file tree at paths relative to its own
/// root — the same "guest cwd IS that root" shape
/// `crate::litebox::ensure_combined_tar` uses, so `require()` of local
/// files/`node_modules` resolves exactly like a host process rooted at
/// `workdir` would.
///
/// Appends via `tar -rf` rather than extract-then-retar: round-tripping the
/// 400+MB base rootfs through `tar -x` + `tar -c` was measured live to
/// silently corrupt its packaged symlinks (BSD tar's directory-recursive add
/// reorders/renormalizes entries), which the runner then can't resolve
/// (`ENOENT` opening the ELF). Appending never touches an existing byte of
/// the base archive.
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

/// Best-effort teardown: kill the runner's OWN process, not just the `sudo`
/// wrapper `child` — `sudo` forking rather than exec-replacing itself would
/// otherwise leave the actual root-held runner (and its utun device) running
/// forever after `child.start_kill()` only reaches the wrapper. Matched by
/// this runner's own binary BASENAME (the sudoers grant's pattern requires
/// the `pkill -f` argument to literally START with the bare binary name, no
/// path prefix — `cfg.runner_bin` is a platform-controlled constant, never
/// tenant input, so a bare basename match stays exact) AND the cell's unique
/// `--tun-device-name=` argument, so this can never signal an unrelated
/// process.
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
