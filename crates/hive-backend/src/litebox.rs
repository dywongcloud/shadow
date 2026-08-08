//! Litebox cell backend — a cell is a single host process run under
//! Microsoft's LiteBox (<https://github.com/microsoft/litebox>), an
//! unprivileged Linux syscall-interception sandbox. Exists for nodes where
//! neither real Firecracker microVMs nor the software-virtualized PVM
//! fallback can safely run (`AGENTS.md`'s "PVM kernels" section: a microVM
//! *boot* can hard-reset a PVM host even after `KVM_CREATE_VM` succeeds —
//! that is exactly the state fc-frankfurt is in today, pinned to
//! `HIVE_FORCE_MOCK=1`, i.e. **zero** isolation). Litebox is a real, measured
//! improvement over that specific baseline — see the security posture note
//! below for what it is not.
//!
//! **Status as of 2026-08-08: process sandboxing and filesystem staging are
//! proven working end to end on fc-frankfurt's real kernel; network-facing
//! serving (`start_function`'s actual production purpose) is NOT yet
//! functional — see "Networking" below. `HIVE_LITEBOX_VERIFIED=1` must stay
//! unset fleet-wide until that gap closes; setting it today would select
//! this backend for real deployments whose HTTP servers would then time out
//! on every request.**
//!
//! ## Mechanism
//!
//! Guest and host share ONE process/address space. The default (and only
//! mechanism this backend uses) is litebox's AOT syscall rewriter: before
//! exec, every `syscall` opcode in the target ELF is statically rewritten to
//! trap into litebox's own dispatcher, which validates/mediates the call
//! before (conditionally) issuing it for real, backstopped by an
//! unprivileged seccomp-bpf allowlist at the actual kernel boundary. This
//! backend shells out to the prebuilt `litebox_runner_linux_userland` CLI
//! binary (`-Z --rewrite-syscalls --forward-env`) rather than linking
//! litebox's Rust crates in-process — the same "depend on a separate binary,
//! not a Cargo dependency" shape [`FirecrackerBackend`] uses for the
//! `firecracker` binary, and it sidesteps needing litebox's still-churning
//! Provider-trait API surface as a compile-time dependency of this crate at
//! all. See `ansible/roles/litebox` for how that binary gets built + staged
//! (litebox ships no releases; it's built from a pinned commit on the host).
//!
//! ## Guest filesystem — verified live, not inferred
//!
//! **The guest sees NOTHING of the host filesystem except what is
//! explicitly staged.** This was proven directly on fc-frankfurt
//! (2026-08-08): a bare invocation of the CLI against a real, existing
//! dynamically-linked binary (`/bin/echo`) failed with "failed to open the
//! ELF file: ENOENT" — not because the program was missing, but because its
//! OWN shared-library dependencies (`ld-linux-x86-64.so.2`, `libc.so.6`)
//! were nowhere in the guest's filesystem and the CLI does not resolve them
//! automatically. A second probe confirmed there is no passthrough at all:
//! a sandboxed `cat` could not read an unstaged file that genuinely existed
//! on the host at the exact same path. The fix, also proven live: pass
//! `--initial-files=<tar>` containing (a) the target's full `ldd` closure at
//! their absolute paths and (b) the deployment's own file tree at paths
//! relative to its own root — the guest's default cwd IS that root, so
//! `node server.js` plus `require()` of local files/`node_modules` resolves
//! exactly like a host process rooted at the build dir would, no path
//! rewriting needed. [`LiteboxBackend::deliver_build`]/`start_function`
//! implement this; there is no compile-cache directory across cold starts
//! for the same reason (the guest fs is a fresh in-memory snapshot every
//! process, nothing persists back to the host).
//!
//! ## Networking — NOT yet functional for ordinary apps (open gap)
//!
//! **`127.0.0.1` loopback is explicitly unsupported by litebox today** —
//! confirmed directly from upstream's own test comment
//! (`litebox_shim_linux/src/syscalls/net.rs`: "We do not support loopback
//! yet"), and reproduced live: a sandboxed process reported itself
//! listening, yet a host connection to `127.0.0.1:<port>` got an immediate
//! ECONNREFUSED with no guest-side syscall ever observed. This breaks EVERY
//! other backend's addressing convention (`CellEndpoint::Tcp("127.0.0.1:
//! <port>")`, what the gateway's tunnel-fronting code dials universally).
//!
//! Real networking needs a TUN device (litebox ships an official one-time
//! setup script, `litebox_platform_linux_userland/scripts/tun-setup.sh` —
//! `ip tuntap add dev tun99 mode tun` + a private `/24`, e.g. `10.0.0.1`
//! host-side, `10.0.0.2` guest-side) passed via `--tun-device-name`. Proven
//! live on fc-frankfurt (2026-08-08) once set up: the host CAN reach the
//! guest's IP — but only when the guest app binds EXPLICITLY to that exact
//! address (`.listen(port, '10.0.0.2', ...)`). A wildcard/`0.0.0.0` bind —
//! what virtually every real Node/Express/Fastify app does by default, and
//! the only bind shape every OTHER backend on this platform requires
//! (`FunctionLaunch`'s own doc: "The process MUST listen on $PORT" — no
//! address requirement) — silently does not work: the app reports itself
//! listening, but the host's connection attempt still gets ECONNREFUSED.
//! This is not an integration bug on this crate's side; it is litebox's
//! current TCP implementation only matching an exact configured address.
//!
//! Two further open questions block a real fix, neither resolved yet:
//! (1) whether litebox's guest stack can be made to accept a wildcard bind
//! at all (would need either an upstream litebox change or a rewrite of the
//! guest's own bind address, which this crate cannot control — the address
//! comes from tenant code); (2) the concurrent-multi-cell IP/TUN model — a
//! `tun-setup.sh`-created device is NOT `IFF_MULTI_QUEUE`, so how (or
//! whether) more than one simultaneously-running cell gets a distinct,
//! reachable guest address is unverified. Until both are resolved,
//! `start_function`'s plain-function path will cold-start the guest process
//! successfully and then time out waiting for it to become reachable —
//! exactly the `DEPLOYMENT_START_FAILED` shape, indistinguishable from a
//! genuinely broken deployment. **Do not set `HIVE_LITEBOX_VERIFIED=1` on
//! any node until this section is updated to say otherwise.**
//!
//! ## Scope: `start_function` only, never `run_build`
//!
//! [`run_build`](LiteboxBackend::run_build) deliberately runs as a **plain,
//! unsandboxed host process** — identical to [`crate::mock::MockBackend`],
//! via the shared `crate::mock::run_build_process`. A build script is
//! fork/exec-heavy (`git clone` forks+execs `git`; `npm install` forks+execs
//! dozens of children), and litebox's own `sys_clone` handler does not
//! support `fork` yet (confirmed directly in upstream source,
//! `litebox_shim_linux/src/syscalls/process.rs`: "exit_signal is ignored
//! because we don't support fork yet; we just validate it"). Wrapping the
//! build shell in litebox today would simply fail on the first forked
//! subprocess. Litebox's isolation is scoped to
//! [`start_function`](LiteboxBackend::start_function)'s single long-lived
//! process instead — the case litebox's own test suite actually exercises
//! (`litebox_runner_linux_userland/tests/loader.rs`'s
//! `test_load_exec_dynamic`/`test_syscall_rewriter`, both confirmed passing
//! on fc-frankfurt's exact kernel) and the one where sandboxing tenant code
//! matters most (it is the process handling live request bytes for the
//! deployment's whole lifetime). A tenant function that itself shells out or
//! calls `child_process.fork()` will fail to start under this backend for
//! the same underlying reason — that surfaces as an ordinary
//! `DEPLOYMENT_START_FAILED` / crash-loop, the existing fault class, not a
//! new failure mode.
//!
//! CONTAINER cells (`func.start_cmd[0] == "__container__"`) bypass litebox
//! entirely and run via host podman (`crate::podman_run_container`, the
//! exact helper [`FirecrackerBackend`] calls) — the platform's existing
//! container path (optionally gVisor `runsc`-sandboxed) is already
//! stronger isolation than litebox provides, so there is nothing to gain by
//! routing it through litebox and a real reason not to (see below).
//!
//! ## Security posture — read before enabling on a node carrying real traffic
//!
//! This is a genuine, measured improvement over `MockBackend` on a node that
//! has no other option: syscalls outside an explicit allowlist are denied at
//! the real kernel boundary (seccomp-bpf), which `MockBackend` does not do at
//! all (it is a bare host process with best-effort `rlimit`s only). It is
//! **not** a substitute for Firecracker/KVM-grade or gVisor-grade isolation,
//! and that is not a hedge — it is upstream's own stated position:
//!
//! - Guest and enforcement code share one address space; there is no
//!   hardware or namespace boundary. Litebox's own doc comment on the
//!   rewriter technique says outright it "should not be considered a
//!   security boundary."
//! - A full sandbox escape was found and fixed very recently upstream
//!   (litebox issue #1006 / PR #1007): `mmap(RW, ANON)` → write a raw
//!   `syscall` opcode into that memory → `mprotect(RX)` → jump to it ran
//!   **unmediated on the host**, because the AOT rewriter only rewrites
//!   opcodes present in the ELF at load time, not ones constructed at
//!   runtime. The same PR fixed a control-flow hijack via `rt_sigreturn`
//!   and an integer-overflow `mremap` giving arbitrary host `munmap`.
//! - Litebox's own stated non-goal: **dynamically generated (e.g.
//!   JIT-emitted) syscalls are not supported** — the rewriter can only
//!   rewrite what's on disk at rewrite time. This is directly relevant to
//!   the actual workload this backend runs: Node.js is built on V8, a JIT.
//!   If V8 ever emits a raw `syscall` opcode as part of its own normal
//!   operation, that instruction executes outside the rewriter's mediation;
//!   the seccomp-bpf allowlist is the only remaining backstop, and it
//!   filters by syscall *number*, not by whether litebox's own
//!   fd/memory-accounting was informed of the call.
//!
//! Consequently this backend must never be silently substituted for
//! Firecracker capability, and selecting it is a deliberate, manual,
//! per-node operator decision — never automatic detection. See
//! `crates/hive-cloud/src/main.rs`'s backend-selection chain and
//! [`LiteboxBackend::smoke_test`]'s doc comment for the two-tier
//! verification this mirrors from the PVM precedent (`AGENTS.md` "PVM
//! kernels (KVM without hardware virt)").

use crate::{CellBackend, CellEndpoint, CellHandle, CellSpec, FunctionLaunch, LogSink};
use async_trait::async_trait;
use hive_core::{now_ms, BuildJob, BuildResult, CellId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

/// Backend config knobs.
#[derive(Clone, Debug)]
pub struct LiteboxConfig {
    /// Path to the prebuilt `litebox_runner_linux_userland` binary. Defaults
    /// to `HIVE_LITEBOX_RUNNER_BIN` if set, else `/usr/local/bin/litebox-runner`
    /// (where `ansible/roles/litebox` installs it).
    pub runner_bin: PathBuf,
    /// Base directory under which per-cell work dirs AND the per-image
    /// `--initial-files` tar cache live. Ephemeral by design — cells are
    /// single-use for builds and re-provisioned from `deliver_build`'s
    /// durable tar artifact for functions — mirrors `MockConfig::root`.
    pub root: PathBuf,
    /// Shared build cache root, passed through to the same
    /// `crate::mock::run_build_process` build pipeline `MockBackend` uses.
    pub cache_root: PathBuf,
    /// Simulated boot latency for a cold provision, matching `MockConfig`'s
    /// shape (warm pools pay this ahead of time). Litebox's real provision
    /// cost is a directory create, not a boot — this exists only so the pool
    /// accounting behaves the same as every other backend; default is 0.
    pub provision_latency: Duration,
}

impl Default for LiteboxConfig {
    fn default() -> Self {
        let runner_bin = std::env::var("HIVE_LITEBOX_RUNNER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/usr/local/bin/litebox-runner"));
        LiteboxConfig {
            runner_bin,
            root: std::env::temp_dir().join("hive-litebox-cells"),
            cache_root: std::env::temp_dir().join("hive-litebox-cache"),
            provision_latency: Duration::from_millis(0),
        }
    }
}

pub struct LiteboxBackend {
    cfg: LiteboxConfig,
    /// Long-lived function processes (the litebox runner itself — guest and
    /// runner are one process), keyed by cell, killed on terminate.
    funcs: Arc<AsyncMutex<HashMap<CellId, tokio::process::Child>>>,
    /// Per-cell tunnel-server accept loops, aborted on terminate.
    tunnels: Arc<AsyncMutex<HashMap<CellId, tokio::task::JoinHandle<()>>>>,
    /// Per-cell podman container name (CONTAINER cells bypass litebox — see
    /// module doc — and run exactly like `FirecrackerBackend`'s container
    /// branch).
    containers: Arc<AsyncMutex<HashMap<CellId, String>>>,
    ctnl_tasks: Arc<AsyncMutex<HashMap<CellId, tokio::task::JoinHandle<()>>>>,
    sampler: Arc<crate::CpuSampler>,
}

impl LiteboxBackend {
    pub fn new(cfg: LiteboxConfig) -> Self {
        LiteboxBackend {
            cfg,
            funcs: Arc::new(AsyncMutex::new(HashMap::new())),
            tunnels: Arc::new(AsyncMutex::new(HashMap::new())),
            containers: Arc::new(AsyncMutex::new(HashMap::new())),
            ctnl_tasks: Arc::new(AsyncMutex::new(HashMap::new())),
            sampler: Arc::new(crate::CpuSampler::new()),
        }
    }

    /// Tier 1: cheap existence probe — mirrors
    /// `FirecrackerBackend::is_supported`'s shape exactly (device/binary
    /// existence only). This is NOT proof the sandbox actually works — see
    /// [`smoke_test`](Self::smoke_test) for the real functional check, and
    /// the module doc's "PVM kernels" cross-reference for why existence
    /// alone is provably insufficient (a KVM host can pass an existence
    /// check and still hard-reset on real use).
    pub fn is_supported(&self) -> bool {
        cfg!(target_os = "linux") && self.cfg.runner_bin.exists()
    }

    fn images_dir(&self) -> PathBuf {
        self.cfg.root.join("litebox-images")
    }

    /// Per-deployment tar of `deliver_build`'s build dir, paths relative to
    /// the build dir's own root (see module doc's "Guest filesystem").
    fn app_tar_path(&self, image: &str) -> PathBuf {
        self.images_dir()
            .join(format!("{}.app.tar", crate::sanitize_image(image)))
    }

    /// Combined per-(image, runtime binary) `--initial-files` tar: the app
    /// tar plus that binary's full `ldd` closure at absolute paths. Cached
    /// on disk, rebuilt only when the app tar is newer (a redeploy) or the
    /// combined tar doesn't exist yet.
    fn combined_tar_path(&self, image: &str, bin: &Path) -> PathBuf {
        let bin_key = bin.to_string_lossy().replace('/', "_");
        self.images_dir().join(format!(
            "{}.{}.tar",
            crate::sanitize_image(image),
            bin_key
        ))
    }

    async fn ensure_combined_tar(&self, image: &str, bin: &Path) -> anyhow::Result<PathBuf> {
        let app_tar = self.app_tar_path(image);
        anyhow::ensure!(
            app_tar.exists(),
            "litebox: no delivered build staged for image {image} — deliver_build must run \
             before start_function (app tar missing at {})",
            app_tar.display()
        );
        let combined = self.combined_tar_path(image, bin);
        let fresh = match (
            tokio::fs::metadata(&combined).await,
            tokio::fs::metadata(&app_tar).await,
        ) {
            (Ok(c), Ok(a)) => c
                .modified()
                .ok()
                .zip(a.modified().ok())
                .map(|(cm, am)| cm >= am)
                .unwrap_or(false),
            _ => false,
        };
        if fresh {
            return Ok(combined);
        }
        let deps = ldd_closure(bin).await?;
        let tmp = combined.with_extension("tar.tmp");
        tokio::fs::copy(&app_tar, &tmp).await.map_err(|e| {
            anyhow::anyhow!("failed to copy {} -> {}: {e}", app_tar.display(), tmp.display())
        })?;
        if !deps.is_empty() {
            let mut cmd = Command::new("tar");
            // `-h`/`--dereference`: a shared-library SONAME (what `ldd`
            // reports, e.g. `libz.so.1`) is very often a symlink to the real
            // versioned file (`libz.so.1.3.1.zlib-ng`) — proven live on
            // fc-frankfurt: without dereferencing, the guest got a symlink
            // node whose relative target was never itself staged, i.e. a
            // dangling link, and `node`'s dynamic linker failed with
            // "cannot open shared object file" on exactly that library.
            // Storing the real bytes under the SONAME name sidesteps the
            // guest ever needing to resolve a symlink target at all.
            cmd.arg("-h")
                .arg("--absolute-names")
                .arg("-rf")
                .arg(&tmp);
            for d in &deps {
                cmd.arg(d);
            }
            let out = cmd
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("failed to run tar: {e}"))?;
            anyhow::ensure!(
                out.status.success(),
                "tar failed staging runtime deps for {}: {}",
                bin.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        tokio::fs::rename(&tmp, &combined).await?;
        Ok(combined)
    }

    /// Tier 2: REAL functional proof — runs a trivial dynamically-linked
    /// program (`/bin/echo`, staged with its real `ldd` closure — see module
    /// doc's "Guest filesystem" section for why an unstaged dependency is a
    /// hard failure, not a degraded one) through the live rewriter and
    /// checks it produced the exact expected output via the sandboxed
    /// process's real stdout.
    ///
    /// **A PASS here proves the syscall-rewriter mechanism only — it does
    /// NOT prove `start_function` can serve a real deployment.** It
    /// deliberately never touches the network, because networking is a
    /// separately-gated, currently-unresolved capability — see the module
    /// doc's "Networking" section. Do not read a PASS as "safe to set
    /// `HIVE_LITEBOX_VERIFIED=1`"; that section states the actual bar.
    ///
    /// **Bring-up only. Never call this against a node already carrying live
    /// traffic** — mirrors `pvm_run_smoke_test`'s gating in `AGENTS.md`
    /// exactly: a smoke test that itself exercises an unproven isolation
    /// path is the kind of check that can wedge the very host serving
    /// traffic. The verdict here is NOT auto-applied to backend selection;
    /// an operator runs this once via the `--litebox-probe` CLI flag during
    /// bring-up — see `main.rs`'s backend-selection chain.
    pub async fn smoke_test(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.is_supported(),
            "litebox runner binary not present at {} (or not on Linux) — nothing to smoke-test",
            self.cfg.runner_bin.display()
        );
        let probe_bin = resolve_bin("echo").await;
        let deps = ldd_closure(&probe_bin).await.unwrap_or_default();
        let tar_path = self.cfg.root.join("litebox-smoke-deps.tar");
        if let Some(parent) = tar_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        if !deps.is_empty() {
            let mut cmd = Command::new("tar");
            // `-h`/`--dereference`: see the identical comment in
            // `ensure_combined_tar` — a SONAME is very often a symlink.
            cmd.arg("-h")
                .arg("--absolute-names")
                .arg("-cf")
                .arg(&tar_path);
            for d in &deps {
                cmd.arg(d);
            }
            let out = cmd
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("failed to run tar: {e}"))?;
            anyhow::ensure!(
                out.status.success(),
                "tar failed staging smoke-test deps: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let marker = format!("litebox-smoke-{}", now_ms());
        let mut cmd = Command::new(&self.cfg.runner_bin);
        cmd.arg("-Z").arg("--rewrite-syscalls");
        if !deps.is_empty() {
            cmd.arg(format!("--initial-files={}", tar_path.display()));
        }
        cmd.arg("--").arg(&probe_bin).arg(&marker);
        let out = cmd
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn litebox runner: {e}"))?;
        anyhow::ensure!(
            out.status.success(),
            "litebox smoke test exited non-zero ({}); stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        anyhow::ensure!(
            stdout.trim() == marker,
            "litebox smoke test ran but stdout did not match — the sandboxed process's output \
             was not reliably delivered to the host. got: {stdout:?}, want: {marker:?}"
        );
        Ok(())
    }
}

impl Default for LiteboxBackend {
    fn default() -> Self {
        LiteboxBackend::new(LiteboxConfig::default())
    }
}

/// Resolve a runtime binary name to an absolute host path the litebox CLI
/// can open directly. The CLI does its own `open()`, not an `execvp`-style
/// PATH search (confirmed from its source: it lexically resolves the
/// program argument relative to CWD, nothing more), so a bare command name
/// like `"node"` (the common `FunctionLaunch::start_cmd[0]` shape) must be
/// resolved here first, the same job the OS shell/`execvp` normally does.
async fn resolve_bin(name: &str) -> PathBuf {
    if name.contains('/') {
        return PathBuf::from(name);
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join(name);
            if tokio::fs::metadata(&candidate)
                .await
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                return candidate;
            }
        }
    }
    PathBuf::from(name)
}

/// The absolute-path shared-library closure of `bin` (interpreter + every
/// `NEEDED` entry `ldd` resolves), skipping the kernel-provided VDSO (no
/// backing file). A genuinely static binary makes `ldd` exit non-zero ("not
/// a dynamic executable") — that means zero deps to stage, not a real
/// error, so it returns `Ok(vec![])` rather than propagating the failure.
async fn ldd_closure(bin: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let out = Command::new("ldd")
        .arg(bin)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run ldd on {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(idx) = line.find("=> ") {
            if let Some(p) = line[idx + 3..].split_whitespace().next() {
                if p.starts_with('/') {
                    paths.push(PathBuf::from(p));
                }
            }
        } else if line.starts_with('/') {
            if let Some(p) = line.split_whitespace().next() {
                paths.push(PathBuf::from(p));
            }
        }
    }
    Ok(paths)
}

#[async_trait]
impl CellBackend for LiteboxBackend {
    fn name(&self) -> &'static str {
        "litebox"
    }

    async fn provision(&self, spec: &CellSpec) -> anyhow::Result<CellHandle> {
        // Identical tenant-jail shape to MockBackend::provision: every cell's
        // sandbox lives under its tenant's subtree, so one tenant's cells can
        // never see another's working files.
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
        tokio::fs::create_dir_all(&root).await?;
        if !self.cfg.provision_latency.is_zero() {
            tokio::time::sleep(self.cfg.provision_latency).await;
        }
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
        // Deliberately NOT sandboxed — see the module doc's "Scope" section:
        // litebox does not support fork() yet, and a build script is
        // fork/exec-heavy by nature. Shares MockBackend's exact pipeline.
        crate::mock::run_build_process(cell, job, sink, &self.cfg.cache_root).await
    }

    /// Packs `build_dir`'s contents into a tar keyed by `image`, paths
    /// relative to `build_dir` itself — see module doc's "Guest filesystem".
    /// `start_function` combines this with the runtime binary's `ldd`
    /// closure into the actual `--initial-files` tar it passes.
    async fn deliver_build(&self, image: &str, build_dir: &std::path::Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            build_dir.is_dir(),
            "deliver_build: build dir does not exist: {}",
            build_dir.display()
        );
        tokio::fs::create_dir_all(self.images_dir()).await?;
        let out = self.app_tar_path(image);
        let tmp = out.with_extension("tar.tmp");
        // `-h`/`--dereference`: some package managers (pnpm's node_modules
        // layout especially) lay out a dependency tree heavily with
        // symlinks; a dangling one is a silent broken `require()` under the
        // same guest-fs constraint documented in `ensure_combined_tar`.
        let res = Command::new("tar")
            .arg("-h")
            .arg("-C")
            .arg(build_dir)
            .arg("-cf")
            .arg(&tmp)
            .arg(".")
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run tar: {e}"))?;
        anyhow::ensure!(
            res.status.success(),
            "tar failed packing {}: {}",
            build_dir.display(),
            String::from_utf8_lossy(&res.stderr).trim()
        );
        tokio::fs::rename(&tmp, &out).await?;
        Ok(())
    }

    fn delivered_workdir(&self) -> Option<&'static str> {
        // The guest's default cwd is its filesystem root, and
        // `deliver_build` tars the build dir at paths relative to that same
        // root (see module doc) — "/" is the litebox analogue of
        // FirecrackerBackend's fixed DELIVERED_WORKDIR guest path.
        Some("/")
    }

    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint> {
        anyhow::ensure!(!func.start_cmd.is_empty(), "empty function start_cmd");

        // CONTAINER cell: bypass litebox entirely, run via host podman — the
        // same helper FirecrackerBackend calls. See module doc for why.
        if func.start_cmd[0] == "__container__" {
            let image = func.start_cmd.get(1).cloned().unwrap_or_default();
            let internal: u16 = func
                .start_cmd
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(8080);
            let net_json = func
                .start_cmd
                .get(3)
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty());
            let runtime = crate::container_runtime();
            let mut ports = vec![crate::ContainerPort::tcp(internal, func.port)];
            ports.extend(func.udp_ports.iter().map(|u| crate::ContainerPort {
                container_port: u.container_port,
                host_port: u.host_port,
                protocol: crate::ContainerProtocol::Udp,
            }));
            let (name, endpoint, task) = crate::podman_run_container(
                &cell.id,
                &image,
                &ports,
                &func.env,
                func.max_concurrency,
                crate::PODMAN_PATH,
                runtime.as_deref(),
                net_json,
                &crate::ContainerLimits::for_container(func.memory_mib, func.cpus, func.pids),
                func.raw_proxy,
                func.gpu,
            )
            .await?;
            self.containers.lock().await.insert(cell.id.clone(), name);
            self.ctnl_tasks.lock().await.insert(cell.id.clone(), task);
            return Ok(endpoint);
        }

        // Plain function: run under litebox's syscall rewriter. `func.workdir`
        // (a HOST path from `deliver_build`'s build_dir) is meaningless to the
        // guest and deliberately ignored here, the same way FirecrackerBackend
        // overwrites it before handing FunctionLaunch to its guest agent — the
        // guest sees `cell.image`'s tar rooted at "/" instead (see module doc).
        let bin = resolve_bin(&func.start_cmd[0]).await;
        let initial_files = self.ensure_combined_tar(&cell.image, &bin).await?;

        let mut cmd = Command::new(&self.cfg.runner_bin);
        cmd.arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--initial-files={}", initial_files.display()))
            .arg("--")
            .arg(&bin)
            .args(&func.start_cmd[1..])
            // Cleared, not inherited: unlike MockBackend's dev-only process
            // spawn, this backend fronts REAL (if lower-trust) tenant
            // traffic — `--forward-env` would otherwise hand this node's own
            // process secrets (HIVE_SECRET_KEY, HIVE_INTERNAL_TOKEN, ...) to
            // sandboxed tenant code. Only the function's own declared env
            // plus the minimum needed to run crosses the boundary.
            .env_clear()
            .env("PORT", func.port.to_string())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/")
            .envs(&func.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn litebox runner at {}: {e}",
                self.cfg.runner_bin.display()
            )
        })?;
        self.funcs.lock().await.insert(cell.id.clone(), child);

        let func_addr = format!("127.0.0.1:{}", func.port);
        if let Err(e) = crate::mock::wait_tcp_ready(&func_addr, Duration::from_secs(15)).await {
            if let Some(mut child) = self.funcs.lock().await.remove(&cell.id) {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            return Err(e);
        }

        // Front the function with a multiplexed tunnel server — same shape
        // as every other backend's serving path.
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
        self.tunnels.lock().await.insert(cell.id.clone(), task);

        Ok(CellEndpoint::Tcp(tunnel_addr))
    }

    async fn terminate(&self, cell: &CellHandle) -> anyhow::Result<()> {
        if let Some(task) = self.tunnels.lock().await.remove(&cell.id) {
            task.abort();
        }
        if let Some(task) = self.ctnl_tasks.lock().await.remove(&cell.id) {
            task.abort();
        }
        if let Some(name) = self.containers.lock().await.remove(&cell.id) {
            crate::podman_stop_container(&name, crate::PODMAN_PATH).await;
        }
        if let Some(mut child) = self.funcs.lock().await.remove(&cell.id) {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        let _ = tokio::fs::remove_dir_all(&cell.root).await;
        Ok(())
    }

    async fn cpu_percent(&self, cell: &CellHandle) -> Option<f32> {
        // Guest and runner are one process (litebox has no separate VMM), so
        // the runner's own PID directly IS the guest's CPU usage — sampling
        // is more direct here than the Firecracker VMM-proxy case.
        let pid = {
            let funcs = self.funcs.lock().await;
            funcs.get(&cell.id).and_then(|c| c.id())?
        };
        self.sampler.cpu_percent(pid, cell.resources.vcpus)
    }
}
