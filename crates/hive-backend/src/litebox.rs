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
//! **Status as of 2026-08-08: `--litebox-probe` PASSES for real on
//! fc-frankfurt, both checks — the syscall rewriter, and a full HTTP round
//! trip through the real per-cell-TUN + patched-litebox + bind-shim
//! pipeline. Beyond the probe, a full `provision` -> `deliver_build` ->
//! `start_function` deployment (a real app with a local `require()`, the
//! exact production code path, not the probe's own inline server) was run
//! live and answered a real `curl` with the correct app-specific response.
//! `HIVE_LITEBOX_VERIFIED` is still NOT set on any node — enabling it for
//! real tenant traffic is a separate, deliberate decision from proving the
//! mechanism works, and this backend still is not Firecracker/gVisor-grade
//! isolation (see "Security posture" below) regardless of how well the
//! mechanics now work.**
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
//! their absolute paths and (b) the deployment's own file tree under
//! `/workspace`. The runner's guest cwd is `/` and exposes no cwd option, so
//! `start_function` validates each Node/Bun main entry against the immutable
//! app archive and passes its exact absolute `/workspace[/app]/...` path; the
//! preload then changes the application cwd to the validated guest workdir
//! before tenant module bytes execute. [`LiteboxBackend::deliver_build`]/
//! `start_function` implement this; there is no compile-cache directory across
//! cold starts for the same reason (the guest fs is a fresh in-memory snapshot
//! every process, nothing persists back to the host).
//!
//! ## Networking
//!
//! Three real constraints, discovered live on fc-frankfurt (2026-08-08) and
//! confirmed by reading litebox's own source, shape everything below:
//!
//! 1. **`127.0.0.1` loopback is architecturally impossible over a TUN
//!    device.** A TUN device is a real point-to-point IP link, not a
//!    loopback interface — this is true of any TUN-isolated stack, not a
//!    litebox defect, and no litebox patch can change it. Confirmed live: a
//!    sandboxed process reported itself listening on `127.0.0.1`; the
//!    host's connection attempt got an immediate ECONNREFUSED with no
//!    guest-side syscall ever observed.
//! 2. **The guest only accepted connections to an EXPLICIT bind address —
//!    litebox's own bug, not a smoltcp limitation.** `litebox/src/net/
//!    mod.rs`'s `bind()`/`listen()` unconditionally built
//!    `smoltcp::wire::IpListenEndpoint { addr: Some(addr), .. }`, even for a
//!    wildcard/omitted-host bind (what real Node/Express apps do by
//!    default). smoltcp itself already fully supports wildcard listening
//!    (`IpListenEndpoint.addr: Option<Address>`, `None` = "any address",
//!    confirmed against smoltcp 0.12.0 — litebox's own exact pinned
//!    version) — litebox's integration code simply never used the sentinel
//!    it was already given the means to use.
//! 3. **The guest's IP and gateway were HARDCODED at compile time, not
//!    configurable per instance** — `litebox/src/net/mod.rs`:
//!    `const INTERFACE_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);` /
//!    `const GATEWAY_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);`, both
//!    already marked `// TODO: Make this configurable` by litebox's own
//!    authors. Every concurrent litebox process therefore claimed the
//!    identical guest address.
//!
//! **Constraints 2 and 3 are fixed with a small, forked patch to litebox
//! itself** (`ansible/roles/litebox/files/networking.patch`, applied by the
//! ansible role right after cloning, before build — see that directory's
//! `PATCHES.md` for the full rationale and exactly what upstream commit
//! it's diffed against): the wildcard-bind sites now map an unspecified
//! address to smoltcp's `None` sentinel instead of `Some(0.0.0.0)`, and
//! `Network::new`/`LinuxShimBuilder::build` gained an additive
//! `_with_addrs`/`_with_net_config` sibling that
//! `litebox_runner_linux_userland` calls, reading `LITEBOX_GUEST_IP`/
//! `LITEBOX_GATEWAY_IP` from the environment (unset = byte-identical to
//! upstream's hardcoded defaults, so every other caller — including
//! litebox's own test suite — needs no changes at all).
//!
//! **With constraint 3 fixed, constraint 1's real solution falls out for
//! free: every cell just gets its own real, directly-routable TUN address,
//! the exact same pattern `FirecrackerBackend::setup_cell_net` already uses
//! for microVMs** (a per-cell `/30`, `net_idx`-allocated — `mode=tun`
//! instead of `mode=tap`, no kernel `ip=` cmdline since litebox reads the
//! two env vars directly instead). No network namespace, veth pair, or
//! DNAT/iptables rule is needed at all — a TUN device is a real
//! point-to-point link, so the moment this host assigns itself the `/30`'s
//! other half, the kernel routes to the guest's address directly:
//!
//! - `provision` allocates a fresh TUN device (`setup_cell_net`,
//!   [`LiteboxNet`]'s doc) with a unique `/30` (e.g. host `10.88.0.1`,
//!   guest `10.88.0.2`).
//! - `start_function` launches the runner directly (no wrapper process)
//!   with `--tun-device-name=<tun>` plus `LITEBOX_GUEST_IP`/
//!   `LITEBOX_GATEWAY_IP` set to this cell's own pair, and the tunnel this
//!   backend fronts the function with dials `net.guest_ip:<port>` directly
//!   — entirely encapsulated here; no other subsystem needs to know a
//!   litebox cell's address scheme differs from `127.0.0.1`.
//! - `terminate` deletes the TUN device; nothing else to clean up.
//!
//! **Loopback (constraint 1) still needs a narrow fix for the minority of
//! apps that explicitly hardcode it** (`.listen(port, '127.0.0.1')` — a
//! wildcard-bind app is already reachable at the guest's real address once
//! the litebox patch is applied, with no shim needed at all).
//! `litebox-bind-shim.js` (embedded via `include_str!`, written fresh
//! before every launch) monkey-patches Node's `net.Server.prototype.
//! _listen2` — the internal, POST-overload-normalization method every real
//! `.listen()` call shape funnels into, a deliberately preserved
//! monkeypatch seam per Node's own source comment, stable across Node
//! v10–v24, and the same technique New Relic's Node agent has run in
//! production since ~2012 — so an explicit loopback (or, belt-and-suspenders,
//! wildcard) bind transparently becomes this cell's real guest address.
//! Zero tenant code changes, preloaded via `NODE_OPTIONS=--require`.
//! Verified against every real `.listen()` shape (options-object form,
//! callback-as-second-arg, unix sockets and fd handles correctly left
//! untouched) with a standalone local test harness before shipping — see
//! that file's own doc comment. **Not yet extended to Python** (the
//! ecosystem is far more heterogeneous — most real Python servers run
//! behind a WSGI/ASGI server like gunicorn/uvicorn, each with its own bind
//! mechanism, unlike Node's tight convergence on `net.Server`) — a Python
//! deployment on this backend will not be reachable until that's built.
//!
//! **Do not wait for litebox's own in-flight rewrite instead of the above.**
//! An unreleased, actively-churning litebox branch (`ulitebox`, not `main`)
//! replaces the whole smoltcp/TUN stack with a broker process issuing real
//! host socket syscalls — genuinely fixing loopback (a real host socket
//! *is* reachable at `127.0.0.1`) — but its own access-control policy
//! (`litebox_broker_core::policy::authorize_socket_bind`) hard-DENIES
//! wildcard binds by design, confirmed by its own unit test. Constraint 2's
//! fix is therefore permanent regardless of which litebox architecture is
//! eventually used, and the branch itself is unstable/undocumented — not a
//! dependency to take today.
//!
//! **Proven live on fc-frankfurt (2026-08-08), not just compiled.**
//! `smoke_test`'s network phase (a real TUN device, the patched litebox, a
//! real Node HTTP server, a real host-side TCP round trip) PASSES, and
//! separately, a full `provision`/`deliver_build`/`start_function`
//! deployment of a real app answered a real `curl` correctly. Getting there
//! took three real bugs found and fixed by live testing, not design review
//! — `setup_cell_net`'s `set -e` aborting on a harmless `ip link del`,
//! litebox's own `SIGINT`/`SIGALRM` disposition assertion tripping under a
//! parent with no controlling terminal, and `wait_tcp_ready`'s per-loop (not
//! per-attempt) deadline check letting one slow `connect()` blow the whole
//! budget — see this crate's git history for the exact fixes. Two of these
//! (the signal assertion, the connect timeout) are general hazards for ANY
//! process this crate spawns over a real network path, not litebox-specific
//! quirks — worth remembering if this pattern gets reused elsewhere.
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

use crate::{
    CellBackend, CellEndpoint, CellHandle, CellSpec, FunctionLaunch, LogSink, SealedRuntimeArtifact,
};
use anyhow::Context as _;
use async_trait::async_trait;
use hive_core::{now_ms, BuildJob, BuildResult, CellId, RuntimeArtifactIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read as StdRead, Seek as StdSeek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
#[cfg(unix)]
use std::{
    os::fd::{AsRawFd, FromRawFd},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;

/// Backend config knobs.
#[derive(Clone, Debug)]
pub struct LiteboxConfig {
    /// Path to the prebuilt `litebox_runner_linux_userland` binary. Defaults
    /// to `HIVE_LITEBOX_RUNNER_BIN` if set, else `/usr/local/bin/litebox-runner`
    /// (where `ansible/roles/litebox` installs it).
    pub runner_bin: PathBuf,
    /// Ephemeral base directory for per-cell runtime scratch and bring-up
    /// probe files. Durable application/runtime archives live under
    /// `cache_root`, never here.
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
            // `root` is per-cell RUNTIME scratch, recreated on every provision
            // and removed on terminate — temp is correct for it.
            root: std::env::temp_dir().join("hive-litebox-cells"),
            // `cache_root` is NOT scratch: it holds the DELIVERED build tar,
            // the one artifact `start_function` cannot run without
            // (`ensure_combined_tar_locked` bails when it is missing). Under
            // `temp_dir()` a reboot or a tmp sweep deletes it while the
            // replicated deployment RECORD survives, so the node then refuses
            // to start a deployment it still believes it hosts — the exact
            // failure AGENTS.md records for `git::deploy_root()`, which was
            // moved off `$TMPDIR` for this reason after it 404'd a live
            // deployment. Same rule, same fix: durable by default, under the
            // node's data dir alongside firecracker's `/var/lib/hive/rootfs`,
            // with an env override for local dev.
            cache_root: std::env::var("HIVE_LITEBOX_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from(
                        std::env::var("HIVE_DATA").unwrap_or_else(|_| "/var/lib/hive".to_string()),
                    )
                    .join("litebox-cache")
                }),
            provision_latency: Duration::from_millis(0),
        }
    }
}

/// Per-cell TUN device + real, distinct guest IP. litebox's guest IP/gateway
/// used to be HARDCODED at compile time (`litebox/src/net/mod.rs`:
/// `INTERFACE_IP_ADDR = 10.0.0.2`, `GATEWAY_IP_ADDR = 10.0.0.1`) — every
/// concurrent cell would otherwise claim the identical guest address.
/// `ansible/roles/litebox/files/networking.patch` fixes this at the source
/// (adds `LITEBOX_GUEST_IP`/`LITEBOX_GATEWAY_IP` env-var overrides, additive
/// and backward-compatible — see that patch's own doc, `files/PATCHES.md`),
/// so each cell can simply be handed its own real `/30` directly, no network
/// namespace or veth pair needed at all — the exact same allocator shape
/// `FirecrackerBackend::setup_cell_net` already uses for microVMs (a per-cell
/// `/30` + TAP device instead of TUN, `net_idx`-derived).
#[derive(Clone)]
struct LiteboxNet {
    /// TUN device name, unique per cell.
    tun_dev: String,
    /// This host's end of the point-to-point link (what the guest's default
    /// route points at).
    host_ip: String,
    /// The guest's own IP on that link — what `start_function`'s tunnel
    /// dials directly; reachable from the host with zero NAT/forwarding,
    /// since a TUN device is a real point-to-point link and the kernel
    /// routes to it the moment `host_ip` is assigned.
    guest_ip: String,
}

struct LiteboxLinkRollback {
    tun_dev: String,
    armed: bool,
}

impl LiteboxLinkRollback {
    fn new(tun_dev: String) -> Self {
        Self {
            tun_dev,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for LiteboxLinkRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let tun_dev = self.tun_dev.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                delete_litebox_link(&tun_dev).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                delete_litebox_link(&tun_dev).await;
            });
        }
    }
}

struct LiteboxProvisionGuard {
    id: CellId,
    root: PathBuf,
    cell_nets: Arc<AsyncMutex<HashMap<CellId, LiteboxNet>>>,
    armed: bool,
}

impl LiteboxProvisionGuard {
    fn new(
        id: CellId,
        root: PathBuf,
        cell_nets: Arc<AsyncMutex<HashMap<CellId, LiteboxNet>>>,
    ) -> Self {
        Self {
            id,
            root,
            cell_nets,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for LiteboxProvisionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let id = self.id.clone();
        let root = self.root.clone();
        let cell_nets = self.cell_nets.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let net = cell_nets.lock().await.remove(&id);
                if let Some(net) = net {
                    delete_litebox_link(&net.tun_dev).await;
                }
                let _ = tokio::fs::remove_dir_all(root).await;
            });
        }
    }
}

async fn delete_litebox_link(tun_dev: &str) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; ip link del {tun_dev} 2>/dev/null"
        ))
        .kill_on_drop(true)
        .status()
        .await;
}

struct LiteboxFunctionProcess {
    child: tokio::process::Child,
    // Keep the exact immutable archive open in the parent for the runner's
    // whole lifetime. The child also inherits its own duplicate, but this
    // prevents parent-side descriptor reuse before termination/wait and makes
    // ownership explicit beside the process that consumes it.
    _initial_files: File,
}

/// One in-flight `exec_command` (Sandboxes): the runner was spawned as the
/// LEADER of its own process group, so `pgid` names the whole guest process
/// tree — the runner plus whatever the guest forks — and `kill_exec` (and
/// `terminate`) end all of it with one `killpg`. On the runner build the
/// fleet runs today a guest `fork()` is NOT a host fork: gdb on a live hang
/// showed the "child" as a second THREAD of the runner (`pgrep -P` empty), so
/// the group is really one process, but the group kill is what stays correct
/// if the emulation ever does become a host fork (the peer's vfork/shared-MM
/// work), and it costs nothing now. Interactive shells are tracked here too,
/// keyed by their session id. Removed by the waiter task the moment the
/// runner is reaped, so a late kill is a no-op.
#[derive(Clone)]
struct LiteboxExec {
    cell: CellId,
    pgid: i32,
}

pub struct LiteboxBackend {
    cfg: LiteboxConfig,
    /// Long-lived function processes (the litebox runner itself — guest and
    /// runner are one process), keyed by cell, killed on terminate.
    funcs: Arc<AsyncMutex<HashMap<CellId, LiteboxFunctionProcess>>>,
    /// Live sandbox execs keyed by `ExecRequest.id` — see [`LiteboxExec`].
    execs: Arc<AsyncMutex<HashMap<String, LiteboxExec>>>,
    /// Per-cell tunnel-server accept loops, aborted on terminate.
    tunnels: Arc<AsyncMutex<HashMap<CellId, tokio::task::JoinHandle<()>>>>,
    /// Per-cell host-container ownership. Container cells bypass litebox and run
    /// through the shared owned launch path.
    containers: Arc<AsyncMutex<HashMap<CellId, crate::ContainerLaunch>>>,
    /// Per-cell TUN device state — see [`LiteboxNet`]. Set
    /// up in `provision` (before the port is known), torn down in
    /// `terminate`. Absent for CONTAINER cells, which never touch this path.
    cell_nets: Arc<AsyncMutex<HashMap<CellId, LiteboxNet>>>,
    /// Monotonic allocator for the per-cell veth `/30` (mirrors
    /// `FirecrackerBackend::net_idx` exactly — same derivation, same 16384
    /// slot space, wraps rather than errors since a wrapped slot is only a
    /// real collision if an old cell at that index is somehow still alive,
    /// which `terminate` prevents).
    net_idx: Arc<std::sync::atomic::AtomicUsize>,
    /// Serializes every reference/artifact publication in this process. A
    /// delivery and a cold start must never write or reap the same immutable
    /// generation concurrently; request-driven acquisition stays outside it.
    artifact_lock: Arc<AsyncMutex<()>>,
    sampler: Arc<crate::CpuSampler>,
}

/// The Node bind-rewrite shim's source, embedded at compile time so the
/// runtime binary is fully self-contained (no separate ansible deploy step
/// to keep in sync) — see `stage_bind_shim`'s doc.
const NODE_BIND_SHIM_JS: &str = include_str!("litebox-bind-shim.js");
/// Where `NODE_OPTIONS=--require` finds the shim INSIDE the guest — a plain
/// filename with no leading path landing at the tar's root when staged
/// (proven live: this is exactly how `deliver_build`'s app tar entries
/// resolve, e.g. `hello.txt` -> guest `/hello.txt`). This is a GUEST path,
/// deliberately distinct from the HOST scratch path `stage_bind_shim` writes
/// the same content to before tar-ing it in — the guest cannot see the host
/// path at all (see module doc, "Guest filesystem"), confirmed live: an
/// earlier version of this code pointed `NODE_OPTIONS` at the host path and
/// every launch failed with `Cannot find module
/// '/tmp/hive-litebox-cells/hive-litebox-bind-shim.js'`.
const GUEST_BIND_SHIM_PATH: &str = "/hive-litebox-bind-shim.js";
const GUEST_RUNTIME_GUARD_PATH: &str = "/hive-litebox-runtime-guard.js";
const INITIAL_FILES_ALIAS_NAME: &str = "initial-files.tar";
const DELIVERED_WORKDIR: &str = "/workspace";
const LITEBOX_ARTIFACT_SCHEMA: u16 = 1;
const RUNTIME_GUARD_EXIT_CODE: i32 = 78;
const MAX_REFERENCE_BYTES: u64 = 1024 * 1024;
const MAX_PACKAGE_JSON_BYTES: u64 = 1024 * 1024;
const MAX_PACKAGE_SCRIPTS: usize = 256;
const MAX_PACKAGE_SCRIPT_BYTES: usize = 16 * 1024;
const MAX_GUEST_ENV_BYTES: usize = 1024 * 1024;
const DEFAULT_ARTIFACT_GC_GRACE_SECS: u64 = 600;
const DEFAULT_ARTIFACT_GC_MAX_REAP_FRACTION: f64 = 0.5;

/// Runs before tenant code in both Node and Bun. The host has already verified
/// the archive hash; this is the guest-side half of the handshake, proving that
/// litebox actually mounted the same protocol/id/content marker before the bind
/// shim changes cwd or any repository byte executes.
const RUNTIME_GUARD_JS: &str = r#"'use strict';
const fs = require('fs');
function refuse(message) {
  try { process.stderr.write(`litebox runtime artifact refusal: ${message}\n`); } catch (_) {}
  process.exit(78);
}
try {
  const marker = JSON.parse(fs.readFileSync('/workspace/.hive-runtime-artifact-v1.json', 'utf8'));
  const protocol = Number(process.env.HIVE_RUNTIME_ARTIFACT_PROTOCOL);
  if (marker.protocol !== protocol ||
      marker.id !== process.env.HIVE_RUNTIME_ARTIFACT_ID ||
      marker.content_sha256 !== process.env.HIVE_RUNTIME_ARTIFACT_SHA256) {
    refuse('mounted identity does not match the host expectation');
  }
} catch (error) {
  refuse(error && error.message ? error.message : String(error));
}
require('/hive-litebox-bind-shim.js');
"#;

static ARTIFACT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeArchiveReference {
    source_sha256: String,
    archive_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiteboxImageReference {
    schema: u16,
    image: String,
    identity: RuntimeArtifactIdentity,
    guest_workdir: String,
    app_archive_sha256: String,
    #[serde(default)]
    package_scripts: BTreeMap<String, String>,
    #[serde(default)]
    next_entry: Option<String>,
    #[serde(default)]
    runtimes: BTreeMap<String, RuntimeArchiveReference>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectRuntime {
    Node,
    Bun,
}

struct DirectLaunch {
    runtime: DirectRuntime,
    bin: PathBuf,
    args: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ArtifactArea {
    Apps,
    Runtimes,
    Temporary,
    Staging,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CacheEntryState {
    device: u64,
    inode: u64,
    mode: u32,
    length: u64,
    modified: SystemTime,
    modified_nanos: i64,
}

impl CacheEntryState {
    fn is_regular(self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
    }

    fn is_directory(self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

struct ArtifactDirectory {
    path: PathBuf,
    descriptor: File,
    identity: DirectoryIdentity,
}

impl ArtifactDirectory {
    fn verify_binding(&self) -> anyhow::Result<()> {
        let current = open_directory_tree(&self.path, false)?;
        anyhow::ensure!(
            current.identity == self.identity,
            "litebox artifact directory identity changed at {}",
            self.path.display()
        );
        Ok(())
    }

    #[cfg(unix)]
    fn open_regular(&self, name: &OsStr) -> std::io::Result<File> {
        let name = cache_component(name)?;
        let fd = unsafe {
            libc::openat(
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cache entry is not a regular file",
            ));
        }
        Ok(file)
    }

    #[cfg(unix)]
    fn create_file(&self, name: &OsStr, mode: u32) -> std::io::Result<File> {
        let name = cache_component(name)?;
        let fd = unsafe {
            libc::openat(
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[cfg(unix)]
    fn child_names(&self) -> anyhow::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(descriptor_directory_path(&self.descriptor))? {
            names.push(entry?.file_name());
        }
        names.sort();
        Ok(names)
    }
}

struct ArtifactDirectories {
    images: ArtifactDirectory,
    apps: ArtifactDirectory,
    runtimes: ArtifactDirectory,
    references: ArtifactDirectory,
    staging: ArtifactDirectory,
    temporary: ArtifactDirectory,
}

impl ArtifactDirectories {
    fn prepare(cache_root: &Path) -> anyhow::Result<Self> {
        let cache = open_directory_tree(cache_root, true)?;
        let images = open_or_create_directory(&cache, OsStr::new("litebox-images-v1"))?;
        let apps = open_or_create_directory(&images, OsStr::new("apps"))?;
        let runtimes = open_or_create_directory(&images, OsStr::new("runtimes"))?;
        let references = open_or_create_directory(&images, OsStr::new("refs"))?;
        let staging = open_or_create_directory(&images, OsStr::new(".artifact-staging"))?;
        let temporary = open_or_create_directory(&images, OsStr::new(".tmp"))?;
        let directories = Self {
            images,
            apps,
            runtimes,
            references,
            staging,
            temporary,
        };
        directories.verify_bindings()?;
        Ok(directories)
    }

    fn verify_bindings(&self) -> anyhow::Result<()> {
        self.images.verify_binding()?;
        self.apps.verify_binding()?;
        self.runtimes.verify_binding()?;
        self.references.verify_binding()?;
        self.staging.verify_binding()?;
        self.temporary.verify_binding()?;
        Ok(())
    }
}

struct ArtifactTemp {
    parent: File,
    name: OsString,
    file: File,
    armed: bool,
}

impl ArtifactTemp {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ArtifactTemp {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        if let Ok(name) = cache_component(&self.name) {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), 0);
            }
        }
    }
}

struct ArtifactScratchAllocation {
    parent: File,
    name: OsString,
    armed: bool,
}

impl Drop for ArtifactScratchAllocation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        if let Ok(name) = cache_component(&self.name) {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
            }
        }
    }
}

struct ArtifactScratch {
    parent: File,
    name: OsString,
    directory: File,
    children: Vec<OsString>,
}

impl Drop for ArtifactScratch {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            for child in &self.children {
                if let Ok(name) = cache_component(child) {
                    unsafe {
                        libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0);
                    }
                }
            }
            if let Ok(name) = cache_component(&self.name) {
                unsafe {
                    libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                }
            }
        }
    }
}

#[cfg(unix)]
fn cache_component(name: &OsStr) -> std::io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid litebox cache path component",
        ));
    }
    CString::new(bytes)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(unix)]
fn directory_identity(file: &File) -> anyhow::Result<DirectoryIdentity> {
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_dir(),
        "litebox cache descriptor is not a directory"
    );
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &OsStr) -> std::io::Result<File> {
    let name = cache_component(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_directory_tree(path: &Path, create: bool) -> anyhow::Result<ArtifactDirectory> {
    anyhow::ensure!(
        path.is_absolute(),
        "litebox artifact cache root must be an absolute path: {}",
        path.display()
    );
    let mut descriptor = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    let mut walked = PathBuf::from("/");
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            _ => anyhow::bail!(
                "litebox artifact cache path is not normalized: {}",
                path.display()
            ),
        };
        walked.push(name);
        let next = match open_directory_at(&descriptor, name) {
            Ok(next) => next,
            Err(error) if create && error.raw_os_error() == Some(libc::ENOENT) => {
                let name_c = cache_component(name)?;
                let rc = unsafe { libc::mkdirat(descriptor.as_raw_fd(), name_c.as_ptr(), 0o700) };
                if rc < 0 {
                    let mkdir_error = std::io::Error::last_os_error();
                    if mkdir_error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(mkdir_error).with_context(|| {
                            format!("create litebox artifact directory {}", walked.display())
                        });
                    }
                } else {
                    descriptor.sync_all().with_context(|| {
                        format!(
                            "sync parent after creating litebox artifact directory {}",
                            walked.display()
                        )
                    })?;
                }
                open_directory_at(&descriptor, name).with_context(|| {
                    format!(
                        "open newly-created litebox artifact directory {}",
                        walked.display()
                    )
                })?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "open no-follow litebox artifact directory {}",
                        walked.display()
                    )
                })
            }
        };
        descriptor = next;
    }
    let identity = directory_identity(&descriptor)?;
    Ok(ArtifactDirectory {
        path: path.to_path_buf(),
        descriptor,
        identity,
    })
}

#[cfg(not(unix))]
fn open_directory_tree(_path: &Path, _create: bool) -> anyhow::Result<ArtifactDirectory> {
    anyhow::bail!("litebox artifact cache requires no-follow Unix directory descriptors")
}

#[cfg(unix)]
fn open_or_create_directory(
    parent: &ArtifactDirectory,
    name: &OsStr,
) -> anyhow::Result<ArtifactDirectory> {
    let path = parent.path.join(name);
    let descriptor = match open_directory_at(&parent.descriptor, name) {
        Ok(descriptor) => descriptor,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            let name_c = cache_component(name)?;
            let rc =
                unsafe { libc::mkdirat(parent.descriptor.as_raw_fd(), name_c.as_ptr(), 0o700) };
            if rc < 0 {
                let mkdir_error = std::io::Error::last_os_error();
                if mkdir_error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(mkdir_error).with_context(|| {
                        format!("create litebox artifact directory {}", path.display())
                    });
                }
            } else {
                parent.descriptor.sync_all().with_context(|| {
                    format!(
                        "sync parent after creating litebox artifact directory {}",
                        path.display()
                    )
                })?;
            }
            open_directory_at(&parent.descriptor, name).with_context(|| {
                format!(
                    "open newly-created litebox artifact directory {}",
                    path.display()
                )
            })?
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "open no-follow litebox artifact directory {}",
                    path.display()
                )
            })
        }
    };
    let identity = directory_identity(&descriptor)?;
    anyhow::ensure!(
        identity.device == parent.identity.device,
        "litebox artifact child directory crosses a filesystem boundary at {}",
        path.display()
    );
    Ok(ArtifactDirectory {
        path,
        descriptor,
        identity,
    })
}

#[cfg(not(unix))]
fn open_or_create_directory(
    _parent: &ArtifactDirectory,
    _name: &OsStr,
) -> anyhow::Result<ArtifactDirectory> {
    anyhow::bail!("litebox artifact cache requires no-follow Unix directory descriptors")
}

#[cfg(target_os = "linux")]
fn descriptor_directory_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(target_os = "macos")]
fn descriptor_directory_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

fn read_bounded_file(mut file: File, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let before = file.metadata()?;
    anyhow::ensure!(
        before.is_file(),
        "litebox cache entry is not a regular file"
    );
    anyhow::ensure!(
        before.len() <= max_bytes,
        "litebox cache entry exceeds {max_bytes} bytes"
    );
    let length = usize::try_from(before.len()).context("litebox cache entry is too large")?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)?;
    let mut extra = [0_u8; 1];
    anyhow::ensure!(
        file.read(&mut extra)? == 0,
        "litebox cache entry grew while reading"
    );
    let after = file.metadata()?;
    #[cfg(unix)]
    anyhow::ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec(),
        "litebox cache entry identity changed while reading"
    );
    Ok(bytes)
}

async fn sha256_open_file(file: &File) -> anyhow::Result<String> {
    let file = file.try_clone()?;
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let before = file.metadata()?;
        anyhow::ensure!(before.is_file(), "litebox artifact is not a regular file");
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 128 * 1024];
        let mut offset = 0_u64;
        while offset < before.len() {
            let take = (before.len() - offset).min(buffer.len() as u64) as usize;
            let mut filled = 0usize;
            while filled < take {
                let read = file.read_at(&mut buffer[filled..take], offset + filled as u64)?;
                anyhow::ensure!(read > 0, "litebox artifact shrank while hashing");
                filled += read;
            }
            hasher.update(&buffer[..take]);
            offset += take as u64;
        }
        let mut extra = [0_u8; 1];
        anyhow::ensure!(
            file.read_at(&mut extra, before.len())? == 0,
            "litebox artifact grew while hashing"
        );
        let after = file.metadata()?;
        #[cfg(unix)]
        anyhow::ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec(),
            "litebox artifact identity changed while hashing"
        );
        Ok(hex_sha256(&hasher.finalize()))
    })
    .await
    .context("litebox artifact hashing task failed")?
}

async fn verify_immutable_open(
    directory: &ArtifactDirectory,
    name: &OsStr,
    expected_sha256: &str,
) -> anyhow::Result<File> {
    anyhow::ensure!(valid_sha256(expected_sha256), "invalid expected SHA-256");
    let file = directory.open_regular(name).with_context(|| {
        format!(
            "open immutable litebox artifact {}/{}",
            directory.path.display(),
            name.to_string_lossy()
        )
    })?;
    let actual = sha256_open_file(&file).await?;
    anyhow::ensure!(
        actual == expected_sha256,
        "artifact SHA-256 mismatch at {}/{} (got {actual}, expected {expected_sha256})",
        directory.path.display(),
        name.to_string_lossy()
    );
    Ok(file)
}

#[cfg(unix)]
fn rename_cache_entry(
    source_parent: &File,
    source_name: &OsStr,
    destination_parent: &File,
    destination_name: &OsStr,
) -> anyhow::Result<()> {
    let source_name = cache_component(source_name)?;
    let destination_name = cache_component(destination_name)?;
    let rc = unsafe {
        libc::renameat(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("rename litebox cache entry");
    }
    Ok(())
}

#[cfg(unix)]
fn link_cache_entry(
    source_parent: &File,
    source_name: &OsStr,
    destination_parent: &File,
    destination_name: &OsStr,
) -> std::io::Result<()> {
    let source_name = cache_component(source_name)?;
    let destination_name = cache_component(destination_name)?;
    let rc = unsafe {
        libc::linkat(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            0,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Hard-link the exact inode named by `source` without consulting its mutable
/// cache pathname. Linux documents `/proc/self/fd/<fd>` plus
/// `AT_SYMLINK_FOLLOW` as the unprivileged `linkat` form for this operation.
#[cfg(target_os = "linux")]
fn link_open_file_alias(
    source: &File,
    destination_parent: &File,
    destination_name: &OsStr,
) -> std::io::Result<()> {
    let source_path = CString::new(format!("/proc/self/fd/{}", source.as_raw_fd()))
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let destination_name = cache_component(destination_name)?;
    let rc = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            source_path.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn link_open_file_alias(
    _source: &File,
    _destination_parent: &File,
    _destination_name: &OsStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "descriptor-bound Litebox archive aliases require Linux procfs",
    ))
}

#[cfg(unix)]
fn unlink_cache_entry(parent: &File, name: &OsStr, directory: bool) -> anyhow::Result<()> {
    let name = cache_component(name)?;
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("unlink litebox cache entry");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn stat_mtime_nanos(stat: &libc::stat) -> i64 {
    stat.st_mtime_nsec
}

#[cfg(target_os = "macos")]
fn stat_mtime_nanos(stat: &libc::stat) -> i64 {
    stat.st_mtime_nsec
}

#[cfg(unix)]
fn cache_entry_state(
    directory: &ArtifactDirectory,
    name: &OsStr,
) -> anyhow::Result<CacheEntryState> {
    let name = cache_component(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            directory.descriptor.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("lstat litebox cache entry");
    }
    let stat = unsafe { stat.assume_init() };
    Ok(CacheEntryState {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
        mode: stat.st_mode as u32,
        length: u64::try_from(stat.st_size).unwrap_or(0),
        modified: SystemTime::UNIX_EPOCH
            + Duration::from_secs(u64::try_from(stat.st_mtime).unwrap_or(0)),
        modified_nanos: stat_mtime_nanos(&stat),
    })
}

fn artifact_area_directory(
    directories: &ArtifactDirectories,
    area: ArtifactArea,
) -> &ArtifactDirectory {
    match area {
        ArtifactArea::Apps => &directories.apps,
        ArtifactArea::Runtimes => &directories.runtimes,
        ArtifactArea::Temporary => &directories.temporary,
        ArtifactArea::Staging => &directories.staging,
    }
}

#[cfg(unix)]
fn remove_cache_entry(
    directory: &ArtifactDirectory,
    name: &OsStr,
    expected: CacheEntryState,
) -> anyhow::Result<()> {
    let current = cache_entry_state(directory, name)?;
    anyhow::ensure!(
        current == expected,
        "litebox artifact GC entry changed between scan and removal: {}/{}",
        directory.path.display(),
        name.to_string_lossy()
    );
    let kind = current.mode & libc::S_IFMT as u32;
    if current.is_directory() {
        let nested_file = open_directory_at(&directory.descriptor, name)?;
        let nested = ArtifactDirectory {
            path: directory.path.join(name),
            identity: directory_identity(&nested_file)?,
            descriptor: nested_file,
        };
        anyhow::ensure!(
            nested.identity.device == current.device && nested.identity.inode == current.inode,
            "litebox artifact GC directory changed while opening {}/{}",
            directory.path.display(),
            name.to_string_lossy()
        );
        remove_cache_directory_contents(&nested)?;
        unlink_cache_entry(&directory.descriptor, name, true)?;
    } else if kind == libc::S_IFREG as u32 || kind == libc::S_IFLNK as u32 {
        unlink_cache_entry(&directory.descriptor, name, false)?;
    } else {
        anyhow::bail!(
            "litebox artifact GC refuses special cache entry {}/{}",
            directory.path.display(),
            name.to_string_lossy()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn remove_cache_directory_contents(directory: &ArtifactDirectory) -> anyhow::Result<()> {
    for name in directory.child_names()? {
        let state = cache_entry_state(directory, &name)?;
        remove_cache_entry(directory, &name, state)?;
    }
    Ok(())
}

impl LiteboxBackend {
    pub fn new(cfg: LiteboxConfig) -> Self {
        // Reap guests orphaned by a previous hive-cloud incarnation. The
        // service runs KillMode=process (load-bearing — see AGENTS.md) so
        // systemd never kills runner children, and the controlled-restart path
        // exits via `process::exit`, which skips every `kill_on_drop`
        // destructor — each restart therefore stranded at least one guest,
        // still bound to its TUN address and still serving its OLD build.
        // When a fresh cell later reused the same net index, the two guests
        // shared one /30 and requests interleaved between builds: corrupted
        // compressed bodies (ERR_CONTENT_DECODING_FAILED in browsers),
        // "function closed before headers" 502s, and stale chunk names —
        // all witnessed live on nodes-wtf 2026-08-26. At construction this
        // process owns every litebox cell on the node and none may be running,
        // so any surviving runner is definitionally stale.
        Self::reap_orphaned_runners(&cfg.runner_bin);
        // The guest network stack services ONE inbound connection at a time
        // (single-listener serial re-arm); overlapping tunnel connects were
        // accept-closed as "function closed before headers". Queue them at the
        // tunnel's connect gate instead — a request waits microseconds rather
        // than failing. Process-wide is correct: a node runs exactly one
        // backend, and every function on a litebox node is a litebox guest.
        fluid_tunnel::set_local_connect_permits(1);
        LiteboxBackend {
            cfg,
            funcs: Arc::new(AsyncMutex::new(HashMap::new())),
            execs: Arc::new(AsyncMutex::new(HashMap::new())),
            tunnels: Arc::new(AsyncMutex::new(HashMap::new())),
            containers: Arc::new(AsyncMutex::new(HashMap::new())),
            cell_nets: Arc::new(AsyncMutex::new(HashMap::new())),
            net_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            artifact_lock: Arc::new(AsyncMutex::new(())),
            sampler: Arc::new(crate::CpuSampler::new()),
        }
    }

    /// SIGKILL every process whose executable is this backend's runner binary.
    /// Boot-time only: after construction, live runners belong to THIS process
    /// and must never be swept. Identification is by `/proc/<pid>/exe` against
    /// the configured runner path (never by name substring, which could match
    /// a tenant process), and this process's own children cannot exist yet.
    fn reap_orphaned_runners(runner_bin: &Path) {
        #[cfg(target_os = "linux")]
        {
            let canonical = runner_bin.canonicalize().ok();
            let Ok(entries) = std::fs::read_dir("/proc") else {
                return;
            };
            let mut reaped = 0u32;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                    continue;
                };
                if pid == std::process::id() {
                    continue;
                }
                let exe = std::fs::read_link(entry.path().join("exe")).ok();
                let matches = match (&exe, &canonical) {
                    (Some(exe), Some(canonical)) => exe == canonical || exe.as_path() == runner_bin,
                    (Some(exe), None) => exe.as_path() == runner_bin,
                    _ => false,
                };
                if matches {
                    // SAFETY: kill(2) with a specific pid and no pointer args.
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                    reaped += 1;
                }
            }
            if reaped > 0 {
                tracing::warn!(
                    reaped,
                    runner = %runner_bin.display(),
                    "reaped litebox guest(s) orphaned by a previous hive-cloud incarnation"
                );
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = runner_bin;
    }

    fn unique_temp_name(&self, label: &str, suffix: &str) -> OsString {
        let id = ARTIFACT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        OsString::from(format!(
            ".{label}-{}-{}-{id}{suffix}",
            std::process::id(),
            now_ms()
        ))
    }

    fn allocate_temp_file(
        &self,
        temporary: &ArtifactDirectory,
        label: &str,
        suffix: &str,
    ) -> anyhow::Result<ArtifactTemp> {
        #[cfg(not(unix))]
        anyhow::bail!("litebox artifact publication requires Unix descriptors");
        #[cfg(unix)]
        for _ in 0..256 {
            let parent = temporary.descriptor.try_clone()?;
            let name = self.unique_temp_name(label, suffix);
            match temporary.create_file(&name, 0o600) {
                Ok(file) => {
                    return Ok(ArtifactTemp {
                        parent,
                        name,
                        file,
                        armed: true,
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("could not allocate a unique litebox artifact temp file")
    }

    fn allocate_scratch_directory(
        &self,
        temporary: &ArtifactDirectory,
        label: &str,
    ) -> anyhow::Result<ArtifactScratch> {
        #[cfg(not(unix))]
        anyhow::bail!("litebox artifact publication requires Unix descriptors");
        #[cfg(unix)]
        for _ in 0..256 {
            let allocation_parent = temporary.descriptor.try_clone()?;
            let name = self.unique_temp_name(label, "");
            let name_c = cache_component(&name)?;
            let rc =
                unsafe { libc::mkdirat(temporary.descriptor.as_raw_fd(), name_c.as_ptr(), 0o700) };
            if rc < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EEXIST) {
                    continue;
                }
                return Err(error).context("create litebox artifact scratch directory");
            }
            let mut allocation = ArtifactScratchAllocation {
                parent: allocation_parent,
                name: name.clone(),
                armed: true,
            };
            let preflight = cache_entry_state(temporary, &name)?;
            let directory = open_directory_at(&temporary.descriptor, &name)
                .context("open litebox artifact scratch directory")?;
            let identity = directory_identity(&directory)?;
            anyhow::ensure!(
                preflight.is_directory()
                    && identity.device == preflight.device
                    && identity.inode == preflight.inode,
                "litebox artifact scratch directory changed while opening"
            );
            let scratch = ArtifactScratch {
                parent: temporary.descriptor.try_clone()?,
                name,
                directory,
                children: Vec::new(),
            };
            allocation.armed = false;
            return Ok(scratch);
        }
        anyhow::bail!("could not allocate a unique litebox artifact scratch directory")
    }

    /// Give Litebox the lexical `.tar` pathname its CLI requires without ever
    /// reopening the content-addressed cache name. The alias is a hard link
    /// created from the already-verified open archive descriptor inside a
    /// private held directory; the returned Drop guard removes both names on
    /// every success, error, and cancellation path.
    fn allocate_initial_files_alias(
        &self,
        temporary: &ArtifactDirectory,
        archive: &File,
    ) -> anyhow::Result<ArtifactScratch> {
        let source_before = archive.metadata()?;
        anyhow::ensure!(
            source_before.is_file(),
            "litebox initial-files descriptor is not a regular file"
        );
        let mut alias = self.allocate_scratch_directory(temporary, "initial-files")?;
        let name = OsString::from(INITIAL_FILES_ALIAS_NAME);
        link_open_file_alias(archive, &alias.directory, &name)
            .context("create descriptor-bound Litebox initial-files alias")?;
        alias.children.push(name.clone());

        #[cfg(unix)]
        {
            let name = cache_component(&name)?;
            let fd = unsafe {
                libc::openat(
                    alias.directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("open descriptor-bound Litebox initial-files alias");
            }
            let linked = unsafe { File::from_raw_fd(fd) };
            let source_after = archive.metadata()?;
            let linked_metadata = linked.metadata()?;
            anyhow::ensure!(
                source_before.dev() == source_after.dev()
                    && source_before.ino() == source_after.ino()
                    && source_before.len() == source_after.len()
                    && source_before.modified().ok() == source_after.modified().ok(),
                "litebox initial-files descriptor changed while binding its private alias"
            );
            anyhow::ensure!(
                linked_metadata.is_file()
                    && linked_metadata.dev() == source_before.dev()
                    && linked_metadata.ino() == source_before.ino()
                    && linked_metadata.len() == source_before.len(),
                "litebox initial-files alias is not bound to the verified archive inode"
            );
        }
        Ok(alias)
    }

    /// Append the two platform-owned preloads to an existing canonical tar.
    /// The helper removes the prior end marker and writes exactly one new marker
    /// after both entries, so later tar readers cannot stop before augmentation.
    async fn stage_bind_shim(
        &self,
        _directories: &ArtifactDirectories,
        archive: &ArtifactTemp,
    ) -> anyhow::Result<()> {
        append_litebox_runtime_augmentation(archive.file.try_clone()?, Vec::new(), None, None).await
    }

    /// Allocate a fresh TUN device + real `/30` for one cell — mirrors
    /// `FirecrackerBackend::setup_cell_net`'s allocator exactly (same
    /// `net_idx`-derived third/base octet split, same 16384-slot space,
    /// `mode=tun` instead of `mode=tap`). No namespace or veth pair: since
    /// `networking.patch` makes litebox's guest IP configurable per
    /// invocation (see [`LiteboxNet`]'s doc), each cell can just own a real,
    /// distinct address directly — the host routes to it the moment the
    /// device's host-side address is assigned, no NAT/forwarding needed.
    /// Returns `None` (never panics/propagates) on any setup failure so a
    /// host missing `ip`/CAP_NET_ADMIN/`/dev/net/tun` degrades to "this cell
    /// has no network" rather than failing the whole cell — the caller
    /// (`provision`) is what turns that into a hard error for non-container
    /// cells, since a networkless litebox cell cannot serve anything.
    async fn setup_cell_net(&self, id: &CellId) -> Option<LiteboxNet> {
        use std::sync::atomic::Ordering;
        let i = self.net_idx.fetch_add(1, Ordering::SeqCst) % 16384;
        let third = ((i >> 6) & 0xff) as u8;
        let base = ((i & 0x3f) as u8) * 4;
        let host_ip = format!("10.88.{third}.{}", base + 1);
        let guest_ip = format!("10.88.{third}.{}", base + 2);
        let tun_dev = format!("lbt{i}");
        let mut rollback = LiteboxLinkRollback::new(tun_dev.clone());

        // Recreate fresh every time (delete any stale device from a prior
        // cell at this index) — same idempotency shape as
        // FirecrackerBackend::setup_cell_net's tap recreation, deliberately
        // WITHOUT `set -e`: `ip link del` on a not-yet-existing device (the
        // overwhelmingly common case — `net_idx` only increases) exits
        // non-zero even with its message silenced by `2>/dev/null`, and
        // `set -e` would abort the whole script right there, before ever
        // reaching `ip tuntap add` — reproduced live on fc-frankfurt
        // (2026-08-08). Bare `&&` chaining (matching
        // FirecrackerBackend::setup_cell_net exactly) makes the harmless
        // `del` failure a non-issue: the script's own exit status is
        // whatever the LAST command in the chain returns.
        let script = format!(
            "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; \
             ip link del {tun_dev} 2>/dev/null; \
             ip tuntap add dev {tun_dev} mode tun && \
             ip addr add {host_ip}/30 dev {tun_dev} && \
             ip link set {tun_dev} up"
        );
        let ok = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .kill_on_drop(true)
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; ip link del {tun_dev} 2>/dev/null"
                ))
                .status()
                .await;
            return None;
        }
        let net = LiteboxNet {
            tun_dev,
            host_ip,
            guest_ip,
        };
        self.cell_nets.lock().await.insert(id.clone(), net.clone());
        rollback.commit();
        Some(net)
    }

    /// Tear down a cell's TUN device (best-effort; no netns/veth/iptables
    /// state exists to clean up alongside it).
    async fn teardown_cell_net(&self, id: &CellId) {
        if let Some(net) = self.cell_nets.lock().await.remove(id) {
            let script = format!(
                "export PATH=/usr/sbin:/sbin:/usr/bin:/bin:$PATH; ip link del {} 2>/dev/null",
                net.tun_dev
            );
            let _ = Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .status()
                .await;
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

    fn image_reference_name(image: &str) -> OsString {
        let key = sha256_parts(&[b"hive-litebox-image-reference-v1\0", image.as_bytes()]);
        OsString::from(format!("{key}.json"))
    }

    fn app_archive_name(sha256: &str) -> OsString {
        OsString::from(format!("{sha256}.tar"))
    }

    fn runtime_archive_name(sha256: &str) -> OsString {
        OsString::from(format!("{sha256}.tar"))
    }

    fn prepare_artifact_dirs(&self) -> anyhow::Result<ArtifactDirectories> {
        ArtifactDirectories::prepare(&self.cfg.cache_root)
    }

    async fn load_image_reference_locked(
        &self,
        directories: &ArtifactDirectories,
        image: &str,
    ) -> anyhow::Result<LiteboxImageReference> {
        directories.verify_bindings()?;
        let name = Self::image_reference_name(image);
        let file = directories.references.open_regular(&name).map_err(|error| {
            anyhow::anyhow!(
                "{}: litebox runtime artifact reference is missing for image {image} at {}/{}: {error}",
                hive_core::fault::NODE_IMAGE_MISSING,
                directories.references.path.display(),
                name.to_string_lossy()
            )
        })?;
        let bytes = read_bounded_file(file, MAX_REFERENCE_BYTES).map_err(|error| {
            anyhow::anyhow!(
                "{}: read litebox runtime artifact reference for image {image}: {error:#}",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        let reference: LiteboxImageReference = serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "{}: decode litebox runtime artifact reference for image {image}: {error}",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        validate_image_reference(&reference, image)?;
        verify_immutable_open(
            &directories.apps,
            &Self::app_archive_name(&reference.app_archive_sha256),
            &reference.app_archive_sha256,
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "{}: litebox app archive validation failed for image {image}: {error:#}",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        directories.verify_bindings()?;
        Ok(reference)
    }

    /// Publish the image's application binding exactly once. A retry may observe
    /// runtime-closure metadata added by a prior cold start, but it can never
    /// replace the immutable application/identity/workdir/package binding.
    async fn write_image_reference_locked(
        &self,
        directories: &ArtifactDirectories,
        reference: &LiteboxImageReference,
    ) -> anyhow::Result<()> {
        validate_image_reference(reference, &reference.image)?;
        let bytes = encoded_image_reference(reference)?;
        verify_immutable_open(
            &directories.apps,
            &Self::app_archive_name(&reference.app_archive_sha256),
            &reference.app_archive_sha256,
        )
        .await?;
        directories.verify_bindings()?;
        let mut temp = self.allocate_temp_file(&directories.temporary, "reference", ".json")?;
        {
            let mut file = tokio::fs::File::from_std(temp.file.try_clone()?);
            file.write_all(&bytes).await?;
            file.sync_all().await?;
        }
        let destination_name = Self::image_reference_name(&reference.image);
        match link_cache_entry(
            &temp.parent,
            &temp.name,
            &directories.references.descriptor,
            &destination_name,
        ) {
            Ok(()) => {
                directories.references.descriptor.sync_all()?;
                let published = directories.references.open_regular(&destination_name)?;
                anyhow::ensure!(
                    read_bounded_file(published, MAX_REFERENCE_BYTES)? == bytes,
                    "litebox image reference changed during first publication"
                );
            }
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                let existing = self
                    .load_image_reference_locked(directories, &reference.image)
                    .await?;
                ensure_same_application_binding(&existing, reference)?;
            }
            Err(error) => return Err(error).context("publish no-replace litebox image reference"),
        }
        unlink_cache_entry(&temp.parent, &temp.name, false)?;
        temp.disarm();
        directories.temporary.descriptor.sync_all()?;
        directories.verify_bindings()?;
        Ok(())
    }

    /// Replace only lazily-derived runtime-closure metadata after re-proving
    /// that the durable application binding is byte-for-byte the same one the
    /// cold start loaded. Cancellation before the rename leaves only a private
    /// Drop-cleaned temp; no different application can reach this path.
    async fn write_runtime_reference_locked(
        &self,
        directories: &ArtifactDirectories,
        expected_binding: &mut LiteboxImageReference,
        runtime_key: &str,
        runtime: RuntimeArchiveReference,
    ) -> anyhow::Result<()> {
        let mut existing = self
            .load_image_reference_locked(directories, &expected_binding.image)
            .await?;
        ensure_same_application_binding(&existing, expected_binding)?;
        verify_immutable_open(
            &directories.runtimes,
            &Self::runtime_archive_name(&runtime.archive_sha256),
            &runtime.archive_sha256,
        )
        .await?;
        if existing.runtimes.get(runtime_key) == Some(&runtime) {
            *expected_binding = existing;
            return Ok(());
        }
        let prior_runtimes = existing.runtimes.clone();
        existing.runtimes.insert(runtime_key.to_string(), runtime);
        validate_image_reference(&existing, &existing.image)?;
        let bytes = encoded_image_reference(&existing)?;

        directories.verify_bindings()?;
        let observed = self
            .load_image_reference_locked(directories, &existing.image)
            .await?;
        ensure_same_application_binding(&observed, &existing)?;
        anyhow::ensure!(
            observed.runtimes == prior_runtimes,
            "litebox runtime reference metadata changed before its serialized update"
        );

        let mut temp = self.allocate_temp_file(&directories.temporary, "reference", ".json")?;
        {
            let mut file = tokio::fs::File::from_std(temp.file.try_clone()?);
            file.write_all(&bytes).await?;
            file.sync_all().await?;
        }
        rename_cache_entry(
            &temp.parent,
            &temp.name,
            &directories.references.descriptor,
            &Self::image_reference_name(&existing.image),
        )?;
        temp.disarm();
        directories.references.descriptor.sync_all()?;
        directories.temporary.descriptor.sync_all()?;
        let published = directories
            .references
            .open_regular(&Self::image_reference_name(&existing.image))?;
        anyhow::ensure!(
            read_bounded_file(published, MAX_REFERENCE_BYTES)? == bytes,
            "litebox runtime reference metadata changed during publication"
        );
        directories.verify_bindings()?;
        *expected_binding = existing;
        Ok(())
    }

    async fn publish_immutable_locked(
        &self,
        directories: &ArtifactDirectories,
        temp: &mut ArtifactTemp,
        destination: &ArtifactDirectory,
        destination_name: &OsStr,
        sha256: &str,
    ) -> anyhow::Result<File> {
        directories.verify_bindings()?;
        #[cfg(unix)]
        temp.file
            .set_permissions(std::fs::Permissions::from_mode(0o400))?;
        temp.file.sync_all()?;
        let actual = sha256_open_file(&temp.file).await?;
        anyhow::ensure!(
            actual == sha256,
            "litebox publication temp hash changed (got {actual}, expected {sha256})"
        );
        match link_cache_entry(
            &temp.parent,
            &temp.name,
            &destination.descriptor,
            destination_name,
        ) {
            Ok(()) => {
                destination.descriptor.sync_all()?;
                unlink_cache_entry(&temp.parent, &temp.name, false)?;
                temp.disarm();
                directories.temporary.descriptor.sync_all()?;
            }
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
            Err(error) => return Err(error).context("publish immutable litebox artifact"),
        }
        let file = verify_immutable_open(destination, destination_name, sha256).await?;
        directories.verify_bindings()?;
        Ok(file)
    }

    async fn package_metadata(
        &self,
        staged: &crate::runtime_artifact::StagedRuntimeArtifact,
        app_rel: &Path,
    ) -> anyhow::Result<(BTreeMap<String, String>, Option<String>)> {
        let package_relative = app_rel.join("package.json");
        let scripts = if let Some(bytes) =
            staged.read_regular(&package_relative, MAX_PACKAGE_JSON_BYTES)?
        {
            let package: serde_json::Value = serde_json::from_slice(&bytes)?;
            let mut scripts = BTreeMap::new();
            if let Some(values) = package.get("scripts").and_then(|value| value.as_object()) {
                anyhow::ensure!(
                    values.len() <= MAX_PACKAGE_SCRIPTS,
                    "selected application declares more than {MAX_PACKAGE_SCRIPTS} package scripts"
                );
                for (name, value) in values {
                    let Some(script) = value.as_str() else {
                        anyhow::bail!(
                            "selected application package script {name:?} is not a string"
                        );
                    };
                    anyhow::ensure!(
                        name.len() <= 128 && script.len() <= MAX_PACKAGE_SCRIPT_BYTES,
                        "selected application package script {name:?} exceeds the litebox metadata limit"
                    );
                    scripts.insert(name.clone(), script.to_string());
                }
            }
            scripts
        } else {
            BTreeMap::new()
        };

        let mut base = app_rel.to_path_buf();
        let next_entry = loop {
            let relative = base.join("node_modules/next/dist/bin/next");
            if staged.is_regular_file(&relative)? {
                break Some(guest_path(&relative)?);
            }
            if !base.pop() {
                break None;
            }
        };
        Ok((scripts, next_entry))
    }

    async fn ensure_combined_tar_locked(
        &self,
        directories: &ArtifactDirectories,
        image: &str,
        runtime_key: &str,
        bin: &Path,
        reference: &mut LiteboxImageReference,
    ) -> anyhow::Result<File> {
        let mut deps = ldd_closure(bin).await?;
        // Modern glibc merged libpthread/libdl/librt/libutil into libc, so the
        // runtime binary's own closure never names them — but native addons in
        // the application tree (e.g. @next/swc-linux-x64-gnu) still declare
        // those legacy sonames as NEEDED, and the guest's dlopen fails with
        // "cannot open shared object file" for stubs every glibc host actually
        // ships. Stage the compat stubs (a few KB each) whenever they exist;
        // absent ones are skipped, not errors.
        for stub in [
            "/lib64/libpthread.so.0",
            "/lib64/libdl.so.2",
            "/lib64/librt.so.1",
            "/lib64/libutil.so.1",
        ] {
            let path = PathBuf::from(stub);
            if !deps.contains(&path)
                && std::path::Path::new(stub)
                    .metadata()
                    .map(|metadata| metadata.is_file())
                    .unwrap_or(false)
            {
                deps.push(path);
            }
        }
        deps.sort();
        deps.dedup();
        let source_sha256 = runtime_source_sha256(
            bin,
            &deps,
            &reference.app_archive_sha256,
            &reference.identity,
            NODE_BIND_SHIM_JS,
            RUNTIME_GUARD_JS,
        )
        .await?;
        if let Some(cached) = reference.runtimes.get(runtime_key) {
            if cached.source_sha256 == source_sha256 {
                if let Ok(file) = verify_immutable_open(
                    &directories.runtimes,
                    &Self::runtime_archive_name(&cached.archive_sha256),
                    &cached.archive_sha256,
                )
                .await
                {
                    return Ok(file);
                }
            }
        }

        let app = verify_immutable_open(
            &directories.apps,
            &Self::app_archive_name(&reference.app_archive_sha256),
            &reference.app_archive_sha256,
        )
        .await?;
        let mut temp = self.allocate_temp_file(&directories.temporary, "runtime", ".tar")?;
        {
            let mut input = tokio::fs::File::from_std(app.try_clone()?);
            let mut output = tokio::fs::File::from_std(temp.file.try_clone()?);
            tokio::io::copy(&mut input, &mut output).await?;
            output.sync_all().await?;
        }
        append_litebox_runtime_augmentation(
            temp.file.try_clone()?,
            deps.clone(),
            Some(reference.identity.clone()),
            Some(bin.to_path_buf()),
        )
        .await
        .with_context(|| format!("failed to augment runtime tar for {}", bin.display()))?;
        temp.file.sync_all()?;
        let after = runtime_source_sha256(
            bin,
            &deps,
            &reference.app_archive_sha256,
            &reference.identity,
            NODE_BIND_SHIM_JS,
            RUNTIME_GUARD_JS,
        )
        .await?;
        anyhow::ensure!(
            after == source_sha256,
            "{}: litebox runtime executable or linked-library closure changed during publication",
            hive_core::fault::NODE_RUNTIME_MISSING
        );
        let archive_sha256 = sha256_open_file(&temp.file).await?;
        let destination_name = Self::runtime_archive_name(&archive_sha256);
        let archive = self
            .publish_immutable_locked(
                directories,
                &mut temp,
                &directories.runtimes,
                &destination_name,
                &archive_sha256,
            )
            .await?;
        self.write_runtime_reference_locked(
            directories,
            reference,
            runtime_key,
            RuntimeArchiveReference {
                source_sha256,
                archive_sha256,
            },
        )
        .await?;
        if let Err(error) = self.gc_artifacts_locked(directories).await {
            tracing::warn!(error = %error, "litebox artifact GC refused or failed");
        }
        anyhow::ensure!(
            reference.image == image,
            "litebox runtime artifact reference changed image during publication"
        );
        Ok(archive)
    }

    async fn gc_artifacts_locked(&self, directories: &ArtifactDirectories) -> anyhow::Result<()> {
        directories.verify_bindings()?;
        let mut keep_apps = BTreeSet::new();
        let mut keep_runtimes = BTreeSet::new();
        for name in directories.references.child_names()? {
            let file = directories
                .references
                .open_regular(&name)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "litebox artifact GC refuses non-file reference {}/{}: {error}",
                        directories.references.path.display(),
                        name.to_string_lossy()
                    )
                })?;
            let bytes = read_bounded_file(file, MAX_REFERENCE_BYTES)?;
            let reference: LiteboxImageReference = serde_json::from_slice(&bytes)?;
            validate_image_reference(&reference, &reference.image)?;
            anyhow::ensure!(
                name == Self::image_reference_name(&reference.image),
                "litebox artifact GC refuses a reference with a mismatched filename: {}",
                name.to_string_lossy()
            );
            let app_name = Self::app_archive_name(&reference.app_archive_sha256);
            verify_immutable_open(&directories.apps, &app_name, &reference.app_archive_sha256)
                .await?;
            keep_apps.insert(app_name);
            for runtime in reference.runtimes.values() {
                let runtime_name = Self::runtime_archive_name(&runtime.archive_sha256);
                verify_immutable_open(
                    &directories.runtimes,
                    &runtime_name,
                    &runtime.archive_sha256,
                )
                .await?;
                keep_runtimes.insert(runtime_name);
            }
        }
        anyhow::ensure!(
            !keep_apps.is_empty(),
            "litebox artifact GC refuses an empty keep set"
        );

        let grace = Duration::from_secs(
            std::env::var("HIVE_LITEBOX_ARTIFACT_GC_GRACE_SECS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_ARTIFACT_GC_GRACE_SECS),
        );
        let max_fraction = std::env::var("HIVE_LITEBOX_ARTIFACT_GC_MAX_REAP_FRACTION")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
            .unwrap_or(DEFAULT_ARTIFACT_GC_MAX_REAP_FRACTION);
        let now = SystemTime::now();
        let mut total = 0usize;
        let mut reap = Vec::new();
        for (area, directory) in [
            (ArtifactArea::Apps, &directories.apps),
            (ArtifactArea::Runtimes, &directories.runtimes),
            (ArtifactArea::Temporary, &directories.temporary),
            (ArtifactArea::Staging, &directories.staging),
        ] {
            for name in directory.child_names()? {
                total += 1;
                if (area == ArtifactArea::Apps && keep_apps.contains(&name))
                    || (area == ArtifactArea::Runtimes && keep_runtimes.contains(&name))
                {
                    continue;
                }
                let state = cache_entry_state(directory, &name)?;
                anyhow::ensure!(
                    !matches!(area, ArtifactArea::Apps | ArtifactArea::Runtimes)
                        || state.is_regular(),
                    "litebox artifact GC refuses a non-file immutable archive {}/{}",
                    directory.path.display(),
                    name.to_string_lossy()
                );
                let old_enough = now
                    .duration_since(state.modified)
                    .ok()
                    .is_some_and(|age| age >= grace);
                if old_enough {
                    reap.push((area, name, state));
                }
            }
        }
        let fraction = reap.len() as f64 / total.max(1) as f64;
        anyhow::ensure!(
            fraction <= max_fraction,
            "litebox artifact GC refuses to reap {}/{} entries ({fraction:.3} > {max_fraction:.3})",
            reap.len(),
            total
        );
        directories.verify_bindings()?;
        let mut touched = BTreeSet::new();
        for (area, name, state) in reap {
            let directory = artifact_area_directory(directories, area);
            remove_cache_entry(directory, &name, state)?;
            touched.insert(area);
        }
        for area in touched {
            artifact_area_directory(directories, area)
                .descriptor
                .sync_all()?;
        }
        directories.verify_bindings()?;
        Ok(())
    }

    /// Retire one exact platform-issued image reference. The backend trait has
    /// no deployment-deletion hook yet; its eventual caller must invoke this
    /// only after the image is absent from live platform state. Archive GC keeps
    /// its empty-set, age and maximum-fraction blast-radius guards unchanged.
    pub async fn retire_image_reference(&self, image: &str) -> anyhow::Result<bool> {
        anyhow::ensure!(
            valid_identity_id(image),
            "runtime artifact image contains an invalid platform identity"
        );
        let _publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let name = Self::image_reference_name(image);
        let file = match directories.references.open_regular(&name) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let bytes = read_bounded_file(file, MAX_REFERENCE_BYTES)?;
        let reference: LiteboxImageReference = serde_json::from_slice(&bytes)?;
        validate_image_reference(&reference, image)?;
        directories.verify_bindings()?;
        unlink_cache_entry(&directories.references.descriptor, &name, false)?;
        directories.references.descriptor.sync_all()?;
        directories.verify_bindings()?;
        if let Err(error) = self.gc_artifacts_locked(&directories).await {
            tracing::warn!(error = %error, "litebox artifact GC refused or failed after reference retirement");
        }
        Ok(true)
    }

    /// Tier 2: REAL functional proof, TWO checks.
    ///
    /// **(a) Rewriter check** — runs a trivial dynamically-linked program
    /// (`/bin/echo`, staged with its real `ldd` closure — see module doc's
    /// "Guest filesystem" section) through the live rewriter and checks it
    /// produced the exact expected output via the sandboxed process's real
    /// stdout.
    ///
    /// **(b) Network check** — sets up a real, throwaway per-cell TUN device
    /// (the exact `LiteboxNet` machinery `provision`/`start_function` use),
    /// runs a real Node HTTP server through the FULL pipeline (patched
    /// litebox + bind-rewrite shim), and makes a real HTTP round trip from
    /// the host. A PASS here is what actually licenses `HIVE_LITEBOX_VERIFIED=1`
    /// — see the module doc's "Networking" section for exactly what this
    /// closes and what it still doesn't cover (Bun/Python bind-rewrite,
    /// sustained concurrency under real load).
    ///
    /// **Bring-up only. Never call this against a node already carrying live
    /// traffic** — mirrors `pvm_run_smoke_test`'s gating in `AGENTS.md`
    /// exactly: a smoke test that itself exercises an unproven isolation
    /// path (here: creates a real network namespace + iptables rules) is
    /// the kind of check that can wedge the very host serving traffic. The
    /// verdict here is NOT auto-applied to backend selection; an operator
    /// runs this once via the `--litebox-probe` CLI flag during bring-up —
    /// see `main.rs`'s backend-selection chain.
    pub async fn smoke_test(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.is_supported(),
            "litebox runner binary not present at {} (or not on Linux) — nothing to smoke-test",
            self.cfg.runner_bin.display()
        );
        self.rewriter_smoke_test().await?;
        self.network_smoke_test().await?;
        Ok(())
    }

    async fn rewriter_smoke_test(&self) -> anyhow::Result<()> {
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
                .kill_on_drop(true)
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
        reset_signal_dispositions_before_exec(&mut cmd);
        cmd.arg("-Z").arg("--rewrite-syscalls");
        if !deps.is_empty() {
            cmd.arg(format!("--initial-files={}", tar_path.display()));
        }
        cmd.arg("--").arg(&probe_bin).arg(&marker);
        let out = cmd
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn litebox runner: {e}"))?;
        anyhow::ensure!(
            out.status.success(),
            "litebox rewriter smoke test exited non-zero ({}); stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        anyhow::ensure!(
            stdout.trim() == marker,
            "litebox rewriter smoke test ran but stdout did not match — the sandboxed process's \
             output was not reliably delivered to the host. got: {stdout:?}, want: {marker:?}"
        );
        Ok(())
    }

    async fn network_smoke_test(&self) -> anyhow::Result<()> {
        let probe_id = CellId::new();
        let net = self.setup_cell_net(&probe_id).await.ok_or_else(|| {
            anyhow::anyhow!(
                "litebox network smoke test: failed to set up a TUN device — needs `ip`, \
                 CAP_NET_ADMIN, and /dev/net/tun"
            )
        })?;
        let result = self.network_smoke_test_inner(&net).await;
        self.teardown_cell_net(&probe_id).await;
        result
    }

    async fn network_smoke_test_inner(&self, net: &LiteboxNet) -> anyhow::Result<()> {
        const PROBE_PORT: u16 = 18080;

        let node_bin = resolve_bin("node").await;
        anyhow::ensure!(
            node_bin.exists(),
            "litebox network smoke test: no `node` binary found on PATH — cannot probe \
             networking without a runtime to serve through it"
        );
        let deps = ldd_closure(&node_bin).await?;
        let _publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let archive = self.allocate_temp_file(&directories.temporary, "network-smoke", ".tar")?;
        // Always build the tar (never conditional on `deps` being non-empty
        // — it always needs at least the bind shim) and always pass
        // `--initial-files`, so the guest can find it.
        let mut tar = Command::new("tar");
        let archive_path = crate::runtime_artifact::inherit_file_path(&mut tar, &archive.file)?;
        let out = tar
            .arg("-h")
            .arg("--absolute-names")
            .arg("-cf")
            .arg(archive_path)
            .args(&deps)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run tar: {e}"))?;
        anyhow::ensure!(
            out.status.success(),
            "tar failed staging node's deps for the network smoke test: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        self.stage_bind_shim(&directories, &archive).await?;
        archive.file.sync_all()?;

        let marker = format!("litebox-net-smoke-{}", now_ms());
        // A wildcard bind (`.listen(PROBE_PORT)`, no host) is the exact case
        // `networking.patch` fixes natively — this proves the patch, not
        // just the TUN plumbing around it. The bind-rewrite shim is ALSO
        // preloaded (harmless no-op for this wildcard case, but proves the
        // shim mechanism itself didn't break — see `LiteboxBackend`'s
        // module doc, "Networking").
        let script = format!(
            "require('http').createServer((q,r)=>{{r.end('{marker}')}}).listen({PROBE_PORT});"
        );

        let mut cmd = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut cmd);
        let initial_files_alias =
            self.allocate_initial_files_alias(&directories.temporary, &archive.file)?;
        let initial_files =
            crate::runtime_artifact::inherit_file_path(&mut cmd, &initial_files_alias.directory)?
                .join(INITIAL_FILES_ALIAS_NAME);
        cmd.arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
            .arg(format!("--initial-files={}", initial_files.display()))
            .arg("--")
            .arg(&node_bin)
            .arg("-e")
            .arg(&script)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/")
            .env("LITEBOX_GUEST_IP", &net.guest_ip)
            .env("LITEBOX_GATEWAY_IP", &net.host_ip)
            .env("NODE_OPTIONS", format!("--require {GUEST_BIND_SHIM_PATH}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("failed to spawn litebox runner for the network smoke test: {e}")
        })?;
        // Drain both pipes concurrently in the background so a chatty guest
        // can never fill the pipe buffer and deadlock the process — captured
        // for the failure message below, not printed live.
        use tokio::io::AsyncReadExt;
        let mut stdout_pipe = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf).await;
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf).await;
            buf
        });

        let addr = format!("{}:{PROBE_PORT}", net.guest_ip);
        if let Err(e) = crate::mock::wait_tcp_ready(&addr, Duration::from_secs(10)).await {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let out = stdout_task.await.unwrap_or_default();
            let err = stderr_task.await.unwrap_or_default();
            return Err(e.context(format!(
                "litebox network smoke test: server never became reachable (guest stdout: {:?}, \
                 stderr: {:?})",
                String::from_utf8_lossy(&out).trim(),
                String::from_utf8_lossy(&err).trim(),
            )));
        }
        drop(initial_files_alias);

        use tokio::io::AsyncWriteExt;
        // Bounded: an unresponsive guest must fail loudly, not hang this
        // probe forever. litebox's own userspace TCP stack has no kernel
        // socket behind it — if the guest process dies mid-response without
        // a chance to send a real FIN over the TUN link, `read_to_end` would
        // otherwise wait for an EOF that can never arrive.
        let round_trip = tokio::time::timeout(Duration::from_secs(10), async {
            let mut stream = tokio::net::TcpStream::connect(&addr).await?;
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                .await?;
            let mut resp = Vec::new();
            stream.read_to_end(&mut resp).await?;
            anyhow::Ok(resp)
        })
        .await
        .map_err(|_| anyhow::anyhow!("litebox network smoke test: HTTP round trip timed out"))
        .and_then(|r| r);
        let _ = child.start_kill();
        let _ = child.wait().await;
        let out = stdout_task.await.unwrap_or_default();
        let err = stderr_task.await.unwrap_or_default();
        let guest_output = format!(
            "guest stdout: {:?}, stderr: {:?}",
            String::from_utf8_lossy(&out).trim(),
            String::from_utf8_lossy(&err).trim(),
        );

        let resp = round_trip.map_err(|e| e.context(guest_output.clone()))?;
        let resp_text = String::from_utf8_lossy(&resp);
        anyhow::ensure!(
            resp_text.contains(&marker),
            "litebox network smoke test: real HTTP round trip completed but the response was \
             wrong — got: {resp_text:?}, want a body containing {marker:?} ({guest_output})"
        );
        Ok(())
    }
}

fn sha256_parts(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hex_sha256(&hasher.finalize())
}

fn hex_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_identity_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_guest_workdir(workdir: &str) -> anyhow::Result<()> {
    let path = Path::new(workdir);
    anyhow::ensure!(path.is_absolute(), "litebox guest workdir is not absolute");
    anyhow::ensure!(
        path == Path::new(DELIVERED_WORKDIR) || path.starts_with(Path::new(DELIVERED_WORKDIR)),
        "litebox guest workdir escapes {DELIVERED_WORKDIR}: {workdir}"
    );
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => normalized.push(value),
            _ => anyhow::bail!("litebox guest workdir is not normalized: {workdir}"),
        }
    }
    anyhow::ensure!(
        normalized.to_string_lossy() == workdir,
        "litebox guest workdir is not canonical: {workdir}"
    );
    Ok(())
}

fn encoded_image_reference(reference: &LiteboxImageReference) -> anyhow::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(reference)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_REFERENCE_BYTES,
        "litebox runtime artifact reference exceeds {MAX_REFERENCE_BYTES} bytes"
    );
    Ok(bytes)
}

fn ensure_same_application_binding(
    existing: &LiteboxImageReference,
    candidate: &LiteboxImageReference,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        existing.schema == candidate.schema
            && existing.image == candidate.image
            && existing.identity == candidate.identity
            && existing.guest_workdir == candidate.guest_workdir
            && existing.app_archive_sha256 == candidate.app_archive_sha256
            && existing.package_scripts == candidate.package_scripts
            && existing.next_entry == candidate.next_entry,
        "litebox image {} is already published with a different immutable application/identity/workdir/package binding",
        candidate.image
    );
    Ok(())
}

fn validate_image_reference(
    reference: &LiteboxImageReference,
    expected_image: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        reference.schema == LITEBOX_ARTIFACT_SCHEMA,
        "{}: unsupported litebox artifact-reference schema {}",
        hive_core::fault::NODE_IMAGE_MISSING,
        reference.schema
    );
    anyhow::ensure!(
        reference.image == expected_image && valid_identity_id(&reference.image),
        "{}: litebox artifact reference does not name the exact image",
        hive_core::fault::NODE_IMAGE_MISSING
    );
    anyhow::ensure!(
        reference.identity.protocol == hive_core::RUNTIME_ARTIFACT_PROTOCOL_VERSION
            && reference.identity.id == reference.image
            && valid_sha256(&reference.identity.content_sha256),
        "{}: litebox artifact reference carries an invalid runtime identity",
        hive_core::fault::NODE_IMAGE_MISSING
    );
    validate_guest_workdir(&reference.guest_workdir).map_err(|error| {
        anyhow::anyhow!(
            "{}: invalid litebox artifact workdir: {error:#}",
            hive_core::fault::NODE_IMAGE_MISSING
        )
    })?;
    anyhow::ensure!(
        valid_sha256(&reference.app_archive_sha256),
        "{}: litebox artifact reference carries an invalid app archive hash",
        hive_core::fault::NODE_IMAGE_MISSING
    );
    anyhow::ensure!(
        reference.package_scripts.len() <= MAX_PACKAGE_SCRIPTS
            && reference.package_scripts.iter().all(|(name, script)| {
                name.len() <= 128 && script.len() <= MAX_PACKAGE_SCRIPT_BYTES
            }),
        "{}: litebox artifact reference carries invalid package-script metadata",
        hive_core::fault::NODE_IMAGE_MISSING
    );
    if let Some(next) = &reference.next_entry {
        validate_guest_workdir(
            Path::new(next)
                .parent()
                .and_then(Path::to_str)
                .unwrap_or_default(),
        )
        .map_err(|error| {
            anyhow::anyhow!(
                "{}: invalid litebox Next entry: {error:#}",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        anyhow::ensure!(
            next.ends_with("/node_modules/next/dist/bin/next"),
            "{}: litebox artifact reference carries a non-canonical Next entry",
            hive_core::fault::NODE_IMAGE_MISSING
        );
    }
    for runtime in reference.runtimes.values() {
        anyhow::ensure!(
            valid_sha256(&runtime.source_sha256) && valid_sha256(&runtime.archive_sha256),
            "{}: litebox artifact reference carries invalid runtime archive hashes",
            hive_core::fault::NODE_IMAGE_MISSING
        );
    }
    Ok(())
}

fn guest_path(relative: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(!relative.is_absolute(), "guest artifact path is absolute");
    let mut path = PathBuf::from(DELIVERED_WORKDIR);
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => path.push(value),
            _ => anyhow::bail!("guest artifact path contains traversal"),
        }
    }
    Ok(path.to_string_lossy().into_owned())
}

async fn runtime_source_sha256(
    bin: &Path,
    deps: &[PathBuf],
    app_archive_sha256: &str,
    identity: &RuntimeArtifactIdentity,
    bind_shim: &str,
    runtime_guard: &str,
) -> anyhow::Result<String> {
    const MAX_RUNTIME_SOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    let mut paths = deps.to_vec();
    paths.push(bin.to_path_buf());
    paths.sort();
    paths.dedup();
    let mut hasher = Sha256::new();
    hasher.update(b"hive-litebox-runtime-source-v1\0");
    hasher.update(app_archive_sha256.as_bytes());
    let identity_bytes = serde_json::to_vec(identity)?;
    hasher.update((identity_bytes.len() as u64).to_le_bytes());
    hasher.update(&identity_bytes);
    hasher.update((bind_shim.len() as u64).to_le_bytes());
    hasher.update(bind_shim.as_bytes());
    hasher.update((runtime_guard.len() as u64).to_le_bytes());
    hasher.update(runtime_guard.as_bytes());
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 128 * 1024];
    for path in paths {
        anyhow::ensure!(
            path.is_absolute(),
            "litebox runtime closure contains a non-absolute path: {}",
            path.display()
        );
        let before = tokio::fs::metadata(&path).await?;
        anyhow::ensure!(
            before.is_file(),
            "litebox runtime closure contains a non-file: {}",
            path.display()
        );
        total = total.saturating_add(before.len());
        anyhow::ensure!(
            total <= MAX_RUNTIME_SOURCE_BYTES,
            "litebox runtime closure exceeds {MAX_RUNTIME_SOURCE_BYTES} bytes"
        );
        let path_bytes = path.to_string_lossy();
        hasher.update((path_bytes.len() as u64).to_le_bytes());
        hasher.update(path_bytes.as_bytes());
        hasher.update(before.len().to_le_bytes());
        let mut file = tokio::fs::File::open(&path).await?;
        let mut remaining = before.len();
        while remaining > 0 {
            let take = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..take]).await?;
            hasher.update(&buffer[..take]);
            remaining -= take as u64;
        }
        let mut extra = [0_u8; 1];
        anyhow::ensure!(
            file.read(&mut extra).await? == 0,
            "litebox runtime source grew while hashing: {}",
            path.display()
        );
        let after = file.metadata().await?;
        anyhow::ensure!(
            before.len() == after.len() && before.modified().ok() == after.modified().ok(),
            "litebox runtime source changed while hashing: {}",
            path.display()
        );
    }
    Ok(hex_sha256(&hasher.finalize()))
}

fn launch_refusal(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: litebox requires one direct Node/Bun process and refused this launch shape: {reason}; no process-manager or backend fallback was attempted",
        hive_core::fault::NODE_BACKEND_UNAVAILABLE
    )
}

/// The one message for "this backend cannot run Bun at all", shared by
/// `start_function`'s entry-point refusal (the authoritative `func.runtime`
/// check, which makes this unreachable in the ordinary case) and the
/// belt-and-braces check right after `resolve_direct_launch` (which would
/// otherwise trust `start_cmd`'s argv-derived runtime alone for a
/// mismatched — `func.runtime == Node` but `start_cmd[0] == "bun"` — launch).
/// A NODE fault, not a launch-shape refusal: this backend genuinely cannot
/// run Bun on ANY node, regardless of what is staged, so it is
/// `NODE_RUNTIME_MISSING` rather than `launch_refusal`'s
/// `NODE_BACKEND_UNAVAILABLE`.
fn bun_unsupported_refusal() -> anyhow::Error {
    anyhow::anyhow!(
        "{}: litebox has no supported Bun runtime on this node — its syscall shim panics on \
         Bun's own readlink(\"/proc/self/fd/3\") boot probe (upstream unimplemented!()), so \
         only Node is supported on this backend (operator remedy: place Bun deployments on a \
         Firecracker or Mock-backed node instead; not an application fault)",
        hive_core::fault::NODE_RUNTIME_MISSING
    )
}

fn command_basename(command: &str) -> &str {
    command.rsplit(['/', '\\']).next().unwrap_or(command)
}

fn host_interpreted_environment_key(key: &str) -> bool {
    key.starts_with("LD_")
        || key.starts_with("DYLD_")
        || key.starts_with("MALLOC_")
        || key.starts_with("LITEBOX_")
        || matches!(
            key,
            "GLIBC_TUNABLES"
                | "GCONV_PATH"
                | "GETCONF_DIR"
                | "LOCPATH"
                | "NLSPATH"
                | "LIBC_FATAL_STDERR_"
                | "LIBC_MALLOC_DEBUG"
                | "RUST_LOG"
                | "RUST_BACKTRACE"
                | "RUST_LIB_BACKTRACE"
                | "RUST_MIN_STACK"
                | "TMPDIR"
                | "TMP"
                | "TEMP"
        )
}

fn platform_environment_key(key: &str) -> bool {
    key.starts_with("HIVE_RUNTIME_")
        || matches!(
            key,
            "PORT" | "PATH" | "HOME" | "NODE_OPTIONS" | "BUN_OPTIONS"
        )
}

fn sanitized_guest_environment(func: &FunctionLaunch) -> anyhow::Result<BTreeMap<String, String>> {
    let mut environment = BTreeMap::new();
    let mut total = 0usize;
    for (key, value) in &func.env {
        anyhow::ensure!(
            !key.is_empty() && !key.contains(['\0', '=']) && !value.contains('\0'),
            "litebox tenant environment contains an invalid process-environment entry"
        );
        if host_interpreted_environment_key(key) || platform_environment_key(key) {
            continue;
        }
        total = total
            .saturating_add(key.len())
            .saturating_add(value.len() + 2);
        anyhow::ensure!(
            total <= MAX_GUEST_ENV_BYTES,
            "litebox tenant environment exceeds {MAX_GUEST_ENV_BYTES} bytes"
        );
        environment.insert(key.clone(), value.clone());
    }
    Ok(environment)
}

fn forwarded_package_args<'a>(args: &'a [String]) -> anyhow::Result<&'a [String]> {
    if args.is_empty() {
        return Ok(args);
    }
    if args.first().map(String::as_str) == Some("--") {
        return Ok(&args[1..]);
    }
    Err(launch_refusal(
        "package-manager arguments are ambiguous without a `--` separator",
    ))
}

fn strict_script_tokens(script: &str, port: u16) -> anyhow::Result<Vec<String>> {
    if script.len() > MAX_PACKAGE_SCRIPT_BYTES || !script.is_ascii() {
        return Err(launch_refusal(
            "the selected package script exceeds the bounded launch-metadata grammar",
        ));
    }
    if script.chars().any(|value| {
        matches!(
            value,
            '\0' | '\n' | '\r' | '\'' | '"' | '\\' | ';' | '&' | '|' | '<' | '>' | '`' | '(' | ')'
        )
    }) {
        return Err(launch_refusal(
            "the selected package script requires shell parsing or command composition",
        ));
    }
    let mut tokens = Vec::new();
    for token in script.split_ascii_whitespace() {
        if token == "$PORT" || token == "${PORT}" {
            tokens.push(port.to_string());
            continue;
        }
        if token
            .chars()
            .any(|value| matches!(value, '$' | '*' | '?' | '[' | ']' | '{' | '}' | '~' | '#'))
        {
            return Err(launch_refusal(
                "the selected package script requires shell expansion",
            ));
        }
        tokens.push(token.to_string());
    }
    if tokens.first().map(String::as_str) == Some("exec") {
        tokens.remove(0);
    }
    if tokens.is_empty() {
        return Err(launch_refusal("the selected package script is empty"));
    }
    Ok(tokens)
}

fn expand_package_manager(
    argv: &[String],
    reference: &LiteboxImageReference,
    port: u16,
) -> anyhow::Result<Vec<String>> {
    let manager = command_basename(&argv[0]);
    if matches!(manager, "npx" | "bunx" | "corepack") {
        return Err(launch_refusal(format!(
            "{manager} is a process-manager wrapper"
        )));
    }
    let (script_name, remaining) = match manager {
        "npm" | "pnpm" | "yarn" => match argv.get(1).map(String::as_str) {
            Some("start") => ("start", forwarded_package_args(&argv[2..])?),
            Some("run") | Some("run-script") => {
                let name = argv.get(2).ok_or_else(|| {
                    launch_refusal(format!("{manager} run is missing a script name"))
                })?;
                (name.as_str(), forwarded_package_args(&argv[3..])?)
            }
            _ => {
                return Err(launch_refusal(format!(
                    "unsupported {manager} invocation; only a named package script can be reduced"
                )))
            }
        },
        "bun" if argv.get(1).map(String::as_str) == Some("run") => {
            let name = argv
                .get(2)
                .ok_or_else(|| launch_refusal("bun run is missing a script name"))?;
            (name.as_str(), forwarded_package_args(&argv[3..])?)
        }
        _ => return Ok(argv.to_vec()),
    };
    let script = reference.package_scripts.get(script_name).ok_or_else(|| {
        launch_refusal(format!(
            "package script {script_name:?} is absent from the validated selected application"
        ))
    })?;
    let mut direct = strict_script_tokens(script, port)?;
    direct.extend(remaining.iter().cloned());
    Ok(direct)
}

fn validated_direct_entry(
    runtime: DirectRuntime,
    entry: &str,
    guest_workdir: &str,
) -> anyhow::Result<String> {
    if entry.is_empty() || entry.contains('\0') || entry.starts_with('-') || entry == "." {
        return Err(launch_refusal(
            "the direct runtime entry must be one explicit module path, not stdin, eval, an option, or a directory",
        ));
    }
    if runtime == DirectRuntime::Node && entry == "inspect" {
        return Err(launch_refusal(
            "Node's inspect client is not a direct application module",
        ));
    }
    if runtime == DirectRuntime::Bun
        && matches!(
            entry,
            "add"
                | "build"
                | "create"
                | "init"
                | "install"
                | "link"
                | "pm"
                | "publish"
                | "remove"
                | "repl"
                | "run"
                | "test"
                | "update"
                | "upgrade"
                | "x"
        )
    {
        return Err(launch_refusal(
            "Bun subcommands and package-script shorthand are not direct entry modules",
        ));
    }

    let path = Path::new(entry);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let mut components = 0usize;
        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(value) => {
                    components += 1;
                    relative.push(value);
                }
                _ => {
                    return Err(launch_refusal(
                        "the direct runtime entry contains path traversal",
                    ))
                }
            }
        }
        anyhow::ensure!(
            components > 0,
            "{}",
            launch_refusal("the direct runtime entry is not artifact-relative")
        );
        Path::new(guest_workdir).join(relative)
    };
    let absolute = absolute.to_str().ok_or_else(|| {
        launch_refusal("the direct runtime entry is not valid UTF-8 after normalization")
    })?;
    validate_guest_workdir(absolute).map_err(|error| {
        launch_refusal(format!(
            "the direct runtime entry escapes the validated artifact: {error:#}"
        ))
    })?;

    if runtime == DirectRuntime::Bun {
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase);
        let path_shaped = entry.contains('/') || entry.starts_with("./");
        anyhow::ensure!(
            path_shaped
                || extension.as_deref().is_some_and(|extension| {
                    matches!(
                        extension,
                        "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx"
                    )
                }),
            "{}",
            launch_refusal(
                "Bun's bare script-name shorthand is ambiguous; use an explicit module path"
            )
        );
    }
    Ok(absolute.to_string())
}

async fn validate_archive_main_entry(archive: &File, guest_entry: &str) -> anyhow::Result<()> {
    let archive_entry = Path::new(guest_entry)
        .strip_prefix("/")
        .map_err(|_| launch_refusal("the direct runtime entry is not an absolute guest path"))?;
    let mut command = Command::new("tar");
    let archive_path = crate::runtime_artifact::inherit_file_path(&mut command, archive)?;
    let output = command
        .arg("-tvf")
        .arg(archive_path)
        .arg("--")
        .arg(archive_entry)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| {
            launch_refusal(format!(
                "could not inspect the immutable application archive: {error}"
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // GNU tar's member-miss is the DEPLOYMENT's fault (its start_cmd names
        // a file the sealed tree never had — the build-time preflight in
        // hive-cloud refuses this before sealing; reaching it here means an
        // artifact sealed by an older binary), so it carries the app-fault
        // marker, never `NODE_BACKEND_UNAVAILABLE`: fluid-compute records that
        // marker as a NODE fault on the pool, blaming this node's backend for
        // a missing tenant file. Any other tar failure (unreadable archive) is
        // still this node's problem and keeps the launch-refusal class.
        if stderr.contains("Not found in archive") {
            anyhow::bail!(
                "{}: main entry {guest_entry:?} does not exist in this deployment's sealed \
                 application tree (the start_cmd names a file that was never committed or \
                 built — fix the entry in fluid.json functions[].start_cmd / package.json \
                 scripts.start, or make the build produce it); not a node fault",
                hive_core::fault::DEPLOYMENT_START_FAILED
            );
        }
        return Err(launch_refusal(format!(
            "could not inspect the immutable application archive for main entry {guest_entry:?}: {}",
            stderr.trim()
        )));
    }
    let entry_kind = output
        .stdout
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace());
    anyhow::ensure!(
        entry_kind == Some(b'-'),
        "{}",
        launch_refusal(format!(
            "main entry {guest_entry:?} is not one regular, symlink-free application file"
        ))
    );
    Ok(())
}

fn validate_bun_arguments(args: &[String]) -> anyhow::Result<()> {
    if args.len() <= 1 {
        return Ok(());
    }
    anyhow::ensure!(
        args.get(1).map(String::as_str) == Some("--"),
        "{}",
        launch_refusal("Bun application arguments require an explicit `--` boundary so runtime flags cannot be smuggled after the entry")
    );
    Ok(())
}

fn validated_next_arguments(
    args: &[String],
    entry: String,
    port: u16,
) -> anyhow::Result<Vec<String>> {
    if args.first().map(String::as_str) != Some("start") {
        return Err(launch_refusal(
            "only the production `next start` server can be reduced",
        ));
    }
    let mut result = vec![entry, "start".to_string()];
    let mut index = 1usize;
    let mut directory_seen = false;
    while index < args.len() {
        let argument = &args[index];
        match argument.as_str() {
            "-p" | "--port" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| launch_refusal("next start port option is missing its value"))?;
                anyhow::ensure!(
                    value == &port.to_string(),
                    "{}",
                    launch_refusal("next start may bind only the platform-assigned port")
                );
                result.push(argument.clone());
                result.push(value.clone());
                index += 2;
            }
            "--keepAliveTimeout" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    launch_refusal("next start keepAliveTimeout is missing its value")
                })?;
                let timeout = value.parse::<u32>().map_err(|_| {
                    launch_refusal("next start keepAliveTimeout is not a bounded integer")
                })?;
                anyhow::ensure!(
                    timeout > 0,
                    "{}",
                    launch_refusal("next start keepAliveTimeout must be positive")
                );
                result.push(argument.clone());
                result.push(value.clone());
                index += 2;
            }
            _ if argument
                .strip_prefix("--port=")
                .is_some_and(|value| value == port.to_string()) =>
            {
                result.push(argument.clone());
                index += 1;
            }
            _ if argument.starts_with("--keepAliveTimeout=") => {
                let value = argument.trim_start_matches("--keepAliveTimeout=");
                let timeout = value.parse::<u32>().map_err(|_| {
                    launch_refusal("next start keepAliveTimeout is not a bounded integer")
                })?;
                anyhow::ensure!(
                    timeout > 0,
                    "{}",
                    launch_refusal("next start keepAliveTimeout must be positive")
                );
                result.push(argument.clone());
                index += 1;
            }
            _ if argument.starts_with('-') => {
                return Err(launch_refusal(format!(
                    "next start option {argument:?} is outside the direct-server allowlist"
                )))
            }
            _ => {
                anyhow::ensure!(
                    !directory_seen,
                    "{}",
                    launch_refusal("next start accepts at most one explicit application directory")
                );
                let path = Path::new(argument);
                anyhow::ensure!(
                    !path.is_absolute()
                        && path.components().all(|component| {
                            matches!(component, Component::Normal(_) | Component::CurDir)
                        }),
                    "{}",
                    launch_refusal("next start application directory escapes the artifact workdir")
                );
                directory_seen = true;
                result.push(argument.clone());
                index += 1;
            }
        }
    }
    Ok(result)
}

async fn resolve_direct_launch(
    func: &FunctionLaunch,
    reference: &LiteboxImageReference,
    app_archive: &File,
) -> anyhow::Result<DirectLaunch> {
    let argv = expand_package_manager(&func.start_cmd, reference, func.port)?;
    let command = argv
        .first()
        .ok_or_else(|| launch_refusal("the reduced command is empty"))?;
    let base = command_basename(command);
    if matches!(
        base,
        "npm" | "pnpm" | "yarn" | "npx" | "bunx" | "corepack" | "turbo"
    ) {
        return Err(launch_refusal(format!(
            "nested process-manager command {base:?} cannot be executed by litebox"
        )));
    }
    let mut args = argv[1..].to_vec();
    let (runtime, name) = match base {
        "node" | "nodejs" => {
            anyhow::ensure!(
                !args.is_empty(),
                "{}",
                launch_refusal("direct Node launch is missing an entry module")
            );
            (DirectRuntime::Node, "node")
        }
        "bun" => {
            anyhow::ensure!(
                !args.is_empty(),
                "{}",
                launch_refusal("direct Bun launch is missing an entry module")
            );
            (DirectRuntime::Bun, "bun")
        }
        "next" => {
            let entry = reference.next_entry.clone().ok_or_else(|| {
                launch_refusal("the validated artifact does not contain a direct Next CLI entry")
            })?;
            args = validated_next_arguments(&args, entry, func.port)?;
            (DirectRuntime::Node, "node")
        }
        _ => {
            return Err(launch_refusal(format!(
                "command {base:?} is not a direct Node/Bun runtime"
            )))
        }
    };
    // `--experimental-strip-types` is the ONE Node runtime flag the platform's
    // own exported-server launcher emits (TypeScript entries). It is a loader
    // toggle with no eval/exec/stdin semantics, so permitting exactly it ahead
    // of the entry does not widen the launch grammar — every other option
    // still refuses via validated_direct_entry's leading-dash check. Without
    // this, litebox refused the platform's OWN launcher shape and every
    // TypeScript exported-server deploy placed on a litebox node failed
    // (witnessed live: examples/express on fc-frankfurt).
    let mut entry_index = 0usize;
    if runtime == DirectRuntime::Node {
        while args
            .get(entry_index)
            .is_some_and(|arg| arg == "--experimental-strip-types")
        {
            entry_index += 1;
        }
        anyhow::ensure!(
            entry_index < args.len(),
            "{}",
            launch_refusal("direct Node launch is missing an entry module after runtime flags")
        );
    }
    let entry = validated_direct_entry(runtime, &args[entry_index], &reference.guest_workdir)?;
    validate_archive_main_entry(app_archive, &entry).await?;
    args[entry_index] = entry;
    if runtime == DirectRuntime::Bun {
        validate_bun_arguments(&args)?;
    }
    let bin = resolve_bin(name).await;
    let bin = tokio::fs::canonicalize(&bin).await.unwrap_or(bin);
    Ok(DirectLaunch { runtime, bin, args })
}

/// Kill-and-reap a still-running guest child on ANY exit from the scope this
/// guard is armed in — a normal `Err` return, an early `?`, or the whole
/// future being DROPPED because the caller gave up (`ColdStartGuard` in
/// `fluid-compute` releases the scheduler's own `provisioning` reservation on
/// exactly that path, but has no reach into this process — a genuinely
/// separate resource at a different layer). `tokio::process::Child`'s own
/// `kill_on_drop` sends the signal synchronously but is documented to NOT
/// reap: the OS keeps the process a zombie until something calls `wait()` on
/// it. Witnessed live on fc-phoenix (2026-08-29): three real zombie
/// `litebox-runner` processes, all direct children of the running hive-cloud
/// PID, accumulated purely from repeated timed-out/abandoned cold starts.
/// `disarm()` on the success path hands the child back for its normal,
/// already-correct long life (`terminate()` reaps it then).
struct ChildReapGuard<'a> {
    child: Option<&'a mut Child>,
}

impl<'a> ChildReapGuard<'a> {
    fn new(child: &'a mut Child) -> Self {
        Self { child: Some(child) }
    }

    /// Reborrow the guarded child. The guard, not this call, owns the
    /// underlying reference for its whole lifetime, so callers may reborrow
    /// through it as many times as needed without ever holding a second
    /// independent `&mut Child`.
    fn child_mut(&mut self) -> &mut Child {
        self.child.as_deref_mut().expect("guard already disarmed")
    }

    fn disarm(mut self) {
        self.child = None;
    }
}

impl Drop for ChildReapGuard<'_> {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let _ = child.start_kill();
        // `Drop` cannot `.await`; a detached task still guarantees the reap
        // happens even when THIS guard's own drop is running inside another
        // future's cancellation teardown (no runtime-context requirement
        // beyond `tokio::spawn` needing a live handle, true everywhere this
        // guard is ever constructed).
        if let Some(pid) = child.id() {
            tokio::spawn(async move {
                // SAFETY: reaping our own just-killed child by raw pid — the
                // `Child` handle itself cannot be moved into this task
                // because it is still borrowed by the guard's caller scope.
                let mut status = 0;
                unsafe {
                    libc::waitpid(pid as libc::pid_t, &mut status, 0);
                }
            });
        }
    }
}

async fn wait_litebox_ready(
    child: &mut Child,
    address: &str,
    budget: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.code() == Some(RUNTIME_GUARD_EXIT_CODE) {
                anyhow::bail!(
                    "{}: litebox guest rejected the exact runtime artifact before tenant execution",
                    hive_core::fault::NODE_IMAGE_MISSING
                );
            }
            anyhow::bail!(
                "{}: litebox direct runtime exited before listening on {address} ({status})",
                hive_core::fault::DEPLOYMENT_START_FAILED
            );
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "{}: litebox direct runtime did not listen on {address} within {}s",
                hive_core::fault::DEPLOYMENT_START_FAILED,
                budget.as_secs()
            );
        }
        let connect_budget = (deadline - now).min(Duration::from_millis(250));
        if tokio::time::timeout(connect_budget, tokio::net::TcpStream::connect(address))
            .await
            .is_ok_and(|result| result.is_ok())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

impl Default for LiteboxBackend {
    fn default() -> Self {
        LiteboxBackend::new(LiteboxConfig::default())
    }
}

/// litebox's own `register_exception_handlers` (`litebox_platform_linux_
/// userland/src/lib.rs`) installs handlers for `SIGINT`/`SIGALRM` and
/// ASSERTS the PREVIOUS disposition was `SIG_DFL` — a real, reproducible
/// crash (`assertion left == right failed: signal 2 handler already
/// installed`) whenever that isn't true, confirmed live on fc-frankfurt
/// (2026-08-08): a process spawned with no controlling terminal (`setsid`,
/// and — the actual production case — any child of a process itself run
/// without a tty, e.g. over SSH without `-t` or under systemd) commonly
/// inherits a non-default SIGINT disposition. `pre_exec` runs in the forked
/// child before exec, the same technique `crate::mock::apply_rlimits`
/// already uses, so litebox always starts from a clean slate regardless of
/// what its parent's own disposition happened to be.
fn reset_signal_dispositions_before_exec(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            for sig in [libc::SIGINT, libc::SIGALRM] {
                if libc::signal(sig, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
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
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run ldd on {}: {e}", bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let diagnostic = format!("{stdout}\n{stderr}");
    if !out.status.success() {
        if diagnostic.contains("not a dynamic executable")
            || diagnostic.contains("statically linked")
        {
            return Ok(Vec::new());
        }
        anyhow::bail!(
            "{}: ldd could not resolve the runtime closure for {}: {}",
            hive_core::fault::NODE_RUNTIME_MISSING,
            bin.display(),
            diagnostic.trim()
        );
    }
    anyhow::ensure!(
        !diagnostic.lines().any(|line| line.contains("=> not found")),
        "{}: the runtime closure for {} has an unresolved shared library: {}",
        hive_core::fault::NODE_RUNTIME_MISSING,
        bin.display(),
        diagnostic.trim()
    );
    let mut paths = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(idx) = line.find("=> ") {
            if let Some(p) = line[idx + 3..].split_whitespace().next() {
                if p.starts_with('/') {
                    let validated = validate_ldd_path(Path::new(p))?;
                    paths.push(validated);
                }
            }
        } else if line.starts_with('/') {
            if let Some(p) = line.split_whitespace().next() {
                let validated = validate_ldd_path(Path::new(p))?;
                paths.push(validated);
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Approved library directories for LDD closure paths. Defense-in-depth:
/// shared-library paths from ldd output must resolve to one of these
/// directories. Paths outside the allowlist are rejected before they ever
/// reach a filesystem operation.
const ALLOWED_LIBRARY_DIRS: &[&str] = &[
    "/usr/lib/",
    "/lib/",
    "/lib64/",
    "/usr/lib64/",
    "/usr/local/lib/",
];

fn validate_ldd_path(path: &Path) -> anyhow::Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve LDD path {}: {e}", path.display()))?;
    let resolved_str = resolved.to_string_lossy();
    let allowed = ALLOWED_LIBRARY_DIRS
        .iter()
        .any(|prefix| resolved_str.starts_with(prefix));
    anyhow::ensure!(
        allowed,
        "LDD path {} resolves to {} which is outside the approved library directory allowlist",
        path.display(),
        resolved.display()
    );
    Ok(path.to_path_buf())
}

async fn append_litebox_runtime_augmentation(
    archive: File,
    deps: Vec<PathBuf>,
    identity: Option<RuntimeArtifactIdentity>,
    runtime_bin: Option<PathBuf>,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        append_litebox_runtime_augmentation_blocking(
            archive,
            &deps,
            identity.as_ref(),
            runtime_bin.as_deref(),
        )
    })
    .await
    .context("litebox runtime tar augmentation task failed")?
}

#[cfg(target_os = "linux")]
fn litebox_tar_header(length: u64, mode: u32) -> anyhow::Result<tar::Header> {
    let mut header = tar::Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_mode(mode & 0o777);
    header.set_size(length);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_username("")?;
    header.set_groupname("")?;
    header.set_cksum();
    Ok(header)
}

#[cfg(target_os = "linux")]
fn append_platform_tar_entry(
    builder: &mut tar::Builder<File>,
    path: &Path,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let mut header = litebox_tar_header(bytes.len() as u64, 0o444)?;
    builder
        .append_data(&mut header, path, bytes)
        .with_context(|| format!("append litebox platform file {}", path.display()))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn append_litebox_runtime_augmentation_blocking(
    mut archive: File,
    deps: &[PathBuf],
    identity: Option<&RuntimeArtifactIdentity>,
    runtime_bin: Option<&Path>,
) -> anyhow::Result<()> {
    const BLOCK_BYTES: u64 = 512;
    const END_MARKER_BLOCKS: u64 = 2;
    let metadata = archive.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.len() >= END_MARKER_BLOCKS * BLOCK_BYTES
            && metadata.len() % BLOCK_BYTES == 0,
        "litebox application authority is not a canonical block-aligned tar"
    );
    // GNU tar's real end-of-archive marker is the minimal two-block
    // (1024-byte) zero terminator required by the format, but `tar -cf`
    // additionally zero-pads the WHOLE archive out to its own blocking
    // factor (a full 20-block/10240-byte record by default) — that extra
    // padding is itself all zero, so a fixed "the terminator is exactly the
    // last 1024 bytes" assumption reads real trailing padding instead of the
    // true terminator whenever the archive's length is not already a
    // multiple of 10240. Truncating at that wrong offset leaves the ACTUAL
    // double-zero-block sitting a few blocks before wherever this function's
    // new entries get appended — every standard reader (GNU tar, Python's
    // `tarfile`, and litebox's own guest-side extractor) stops at that real
    // terminator and never sees the entries appended after it, so the guest
    // silently never gets the bind shim it needs.
    //
    // Reproduced live on fc-sanjose-3 AND fc-phoenix (`--litebox-probe`
    // failing on both with `Cannot find module '/hive-litebox-bind-shim.js'`
    // despite `append_litebox_runtime_augmentation_blocking` returning `Ok`
    // and genuinely growing the file — the new headers were real bytes on
    // disk, just placed after the archive's true EOF marker). Fixed by
    // scanning backward from the end in 512-byte blocks for the actual start
    // of the trailing zero run, rather than trusting a fixed offset.
    let total_blocks = metadata.len() / BLOCK_BYTES;
    let mut zero_block = [0_u8; BLOCK_BYTES as usize];
    let mut terminator_start_block = total_blocks;
    let mut index = total_blocks;
    while index > 0 {
        let candidate = index - 1;
        archive.seek(SeekFrom::Start(candidate * BLOCK_BYTES))?;
        archive.read_exact(&mut zero_block)?;
        if zero_block.iter().any(|byte| *byte != 0) {
            break;
        }
        terminator_start_block = candidate;
        index = candidate;
    }
    anyhow::ensure!(
        total_blocks - terminator_start_block >= END_MARKER_BLOCKS,
        "litebox application authority has no exact two-block tar terminator"
    );
    let prefix_bytes = terminator_start_block * BLOCK_BYTES;
    archive.set_len(prefix_bytes)?;
    archive.seek(SeekFrom::Start(prefix_bytes))?;

    let root = File::open("/").context("open root filesystem")?;
    let mut seen_inodes: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut builder = tar::Builder::new(archive);
    builder.mode(tar::HeaderMode::Deterministic);
    builder.follow_symlinks(false);
    for dep in deps {
        let relative = dep
            .strip_prefix("/")
            .map_err(|_| anyhow::anyhow!("LDD path {} is not absolute", dep.display()))?;
        anyhow::ensure!(
            !relative.as_os_str().is_empty(),
            "LDD path is the root directory"
        );
        let mut file = crate::runtime_artifact::openat2_required(
            &root,
            relative.as_os_str(),
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
            crate::runtime_artifact::RESOLVE_BENEATH
                | crate::runtime_artifact::RESOLVE_NO_MAGICLINKS,
        )
        .with_context(|| format!("openat2 failed for {}", dep.display()))?;
        let resolved = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .canonicalize()
            .with_context(|| format!("cannot resolve real path for {}", dep.display()))?;
        let resolved_str = resolved.to_string_lossy();
        anyhow::ensure!(
            ALLOWED_LIBRARY_DIRS
                .iter()
                .any(|prefix| resolved_str.starts_with(prefix)),
            "LDD path {} resolves to {} which is outside the approved library directory allowlist",
            dep.display(),
            resolved.display()
        );
        let before = file
            .metadata()
            .with_context(|| format!("cannot stat {}", dep.display()))?;
        anyhow::ensure!(
            before.is_file(),
            "LDD path is not a file: {}",
            dep.display()
        );
        anyhow::ensure!(
            seen_inodes.insert((before.dev(), before.ino())),
            "duplicate LDD inode detected while staging {} (dev={}, ino={})",
            dep.display(),
            before.dev(),
            before.ino()
        );
        let mut header = litebox_tar_header(before.len(), before.mode())?;
        builder
            .append_data(&mut header, relative, &mut file)
            .with_context(|| format!("append linked library {}", dep.display()))?;
        let after = file.metadata()?;
        anyhow::ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.mode() == after.mode()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec(),
            "linked library changed while staging: {}",
            dep.display()
        );
    }

    // The runner loads the INITIAL program ELF from the host, so a
    // single-process guest (the network smoke test's `node -e`) runs without
    // the binary staged — but a guest-side `execve` (Next.js `next start`
    // spawning its server worker, any app child process re-execing
    // `process.execPath`) resolves the executable through the GUEST
    // filesystem, which is fully separate from the host by design. Without
    // the runtime binary staged at its exact launch path, every such spawn
    // died with the runner's bare "failed to open the ELF file: ENOENT" —
    // witnessed live on nodes-wtf (Next 16). Same openat2/no-follow staging
    // discipline as the library closure above; the executable is exempt from
    // the library-directory allowlist (it legitimately lives in /usr/bin,
    // /usr/local/bin, or a version-manager prefix) but must still be a real
    // regular file reached without following a magic link.
    if let Some(bin) = runtime_bin {
        let relative = bin
            .strip_prefix("/")
            .map_err(|_| anyhow::anyhow!("runtime binary {} is not absolute", bin.display()))?;
        anyhow::ensure!(
            !relative.as_os_str().is_empty(),
            "runtime binary path is the root directory"
        );
        let mut file = crate::runtime_artifact::openat2_required(
            &root,
            relative.as_os_str(),
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
            crate::runtime_artifact::RESOLVE_BENEATH
                | crate::runtime_artifact::RESOLVE_NO_MAGICLINKS,
        )
        .with_context(|| format!("openat2 failed for runtime binary {}", bin.display()))?;
        let before = file
            .metadata()
            .with_context(|| format!("cannot stat runtime binary {}", bin.display()))?;
        anyhow::ensure!(
            before.is_file(),
            "runtime binary is not a file: {}",
            bin.display()
        );
        if seen_inodes.insert((before.dev(), before.ino())) {
            let mut header = litebox_tar_header(before.len(), before.mode())?;
            builder
                .append_data(&mut header, relative, &mut file)
                .with_context(|| format!("append runtime binary {}", bin.display()))?;
            let after = file.metadata()?;
            anyhow::ensure!(
                before.dev() == after.dev()
                    && before.ino() == after.ino()
                    && before.len() == after.len()
                    && before.mode() == after.mode()
                    && before.mtime() == after.mtime()
                    && before.mtime_nsec() == after.mtime_nsec(),
                "runtime binary changed while staging: {}",
                bin.display()
            );
        }
    }

    if let Some(identity) = identity {
        let identity_bytes = serde_json::to_vec(identity)?;
        append_platform_tar_entry(
            &mut builder,
            &Path::new("workspace").join(hive_core::RUNTIME_ARTIFACT_MARKER_FILE),
            &identity_bytes,
        )?;
    }
    append_platform_tar_entry(
        &mut builder,
        Path::new(GUEST_BIND_SHIM_PATH.trim_start_matches('/')),
        NODE_BIND_SHIM_JS.as_bytes(),
    )?;
    append_platform_tar_entry(
        &mut builder,
        Path::new(GUEST_RUNTIME_GUARD_PATH.trim_start_matches('/')),
        RUNTIME_GUARD_JS.as_bytes(),
    )?;
    builder
        .finish()
        .context("finish litebox combined runtime tar")?;
    let archive = builder
        .into_inner()
        .context("close litebox combined runtime tar writer")?;
    archive.sync_all()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn append_litebox_runtime_augmentation_blocking(
    _archive: File,
    _deps: &[PathBuf],
    _identity: Option<&RuntimeArtifactIdentity>,
    _runtime_bin: Option<&Path>,
) -> anyhow::Result<()> {
    anyhow::bail!("descriptor-relative tar augmentation requires Linux openat2")
}

/// The interactive-shell binary plus a small, fixed coreutils set — every
/// program the SHELL ITSELF might `exec()` inside the guest (`fork()`+`exec()`
/// each, on the `AnEntrypoint/litebox` base this platform tracks: see
/// `ansible/roles/litebox/files/PATCHES.md`'s "litebox fork tracking"
/// section) must be staged into the guest tar with its exact host path,
/// same as `append_litebox_runtime_augmentation_blocking`'s own comment on
/// `runtime_bin` explains: only the FIRST exec'd binary loads from the host
/// directly, every later exec (from inside the now-separate guest
/// filesystem) resolves through what's actually staged here. Deliberately
/// small and fixed rather than "whatever's on $PATH" — a sandbox shell that
/// can't find `grep` fails loudly and obviously; one that can silently run
/// anything the host happens to have installed is a much larger, unaudited
/// surface for a tenant-facing feature.
const SHELL_GUEST_PROGRAMS: &[&str] = &[
    "/bin/sh",
    "/usr/bin/ls",
    "/usr/bin/cat",
    "/usr/bin/pwd",
    "/usr/bin/echo",
    "/usr/bin/mkdir",
    "/usr/bin/rm",
    "/usr/bin/cp",
    "/usr/bin/mv",
    "/usr/bin/grep",
    "/usr/bin/head",
    "/usr/bin/tail",
    "/usr/bin/wc",
    "/usr/bin/find",
    "/usr/bin/env",
];

/// Staged only where the host has them — an absent one is skipped with a
/// debug line, never an exec failure (the fixed set above stays strict): the
/// rest of the POSIX tool set a shell one-liner reaches for. Still a closed,
/// audited list, never "whatever is on `$PATH`". This is hang-avoidance as
/// much as convenience: under the litebox fork emulation a command the guest
/// cannot find is the WORST case, not a harmless "not found" — the forked
/// child that would print the error touches glibc state the fork copied
/// wrongly and can spin forever (`litebox-fork-child-corruption`; measured:
/// `sh -c 'uname -a; id'` with neither staged = a runner at 100% CPU until
/// killed). Every entry is a real file or a symlink the staging `openat2`
/// dereferences (Rocky's alternatives-managed `awk`), staged under its
/// `/usr/bin` path.
const SHELL_GUEST_OPTIONAL_PROGRAMS: &[&str] = &[
    "/usr/bin/uname",
    "/usr/bin/id",
    "/usr/bin/whoami",
    "/usr/bin/hostname",
    "/usr/bin/printf",
    "/usr/bin/true",
    "/usr/bin/false",
    "/usr/bin/test",
    "/usr/bin/[",
    "/usr/bin/sleep",
    "/usr/bin/date",
    "/usr/bin/touch",
    "/usr/bin/ln",
    "/usr/bin/chmod",
    "/usr/bin/sed",
    "/usr/bin/awk",
    "/usr/bin/sort",
    "/usr/bin/uniq",
    "/usr/bin/cut",
    "/usr/bin/tr",
    "/usr/bin/xargs",
    "/usr/bin/tee",
    "/usr/bin/basename",
    "/usr/bin/dirname",
    "/usr/bin/stat",
    "/usr/bin/du",
    "/usr/bin/df",
    "/usr/bin/readlink",
    "/usr/bin/realpath",
    "/usr/bin/seq",
    "/usr/bin/sha256sum",
    "/usr/bin/md5sum",
    "/usr/bin/base64",
    "/usr/bin/which",
    "/usr/bin/tar",
    "/usr/bin/gzip",
];

/// Host directories a bare `exec_command` name (`node`, `ls`, `python3`) is
/// resolved against — the guest `PATH` this backend hands the runner, plus
/// `/usr/local/bin` for operator-installed runtimes. The resolved binary is
/// STAGED into the guest tar (with its `ldd` closure) exactly like the fixed
/// shell set, so a command that exists on the host runs in the guest; one
/// that does not fails loudly at start, never mid-run with a bare ENOENT.
const EXEC_PROGRAM_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/local/bin"];

/// Upper bound on one `ExecOutput` line's bytes before it is truncated (the
/// provider bounds again at 16 KiB); keeps a newline-free firehose from
/// growing the pump's buffer without limit.
const EXEC_MAX_LINE_BYTES: usize = 64 * 1024;

/// Resolve the program a sandbox `exec_command` names to the HOST binary the
/// runner will load (the first exec'd ELF loads from the host; every later
/// guest-side `execve` resolves through the staged tar, which is why the
/// caller stages this same path). A relative path is refused: the guest's
/// cwd and the host's are different filesystems by design.
fn resolve_guest_program(cmd: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        !cmd.trim().is_empty() && !cmd.contains('\0'),
        "sandbox command must be a non-empty program name"
    );
    if cmd.contains('/') {
        let path = PathBuf::from(cmd);
        anyhow::ensure!(
            path.is_absolute(),
            "sandbox command path must be absolute inside a litebox guest: {cmd}"
        );
        anyhow::ensure!(
            path.is_file(),
            "command {cmd} is not available inside this litebox sandbox"
        );
        return Ok(path);
    }
    for dir in EXEC_PROGRAM_DIRS {
        let candidate = Path::new(dir).join(cmd);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "command '{cmd}' is not available inside this litebox sandbox (searched {})",
        EXEC_PROGRAM_DIRS.join(":")
    )
}

/// Pump one exec pipe into `ExecOutput` events, one per line, each bounded
/// to `EXEC_MAX_LINE_BYTES`. Keeps DRAINING (discarding) after the receiver
/// goes away: a guest blocked on a full pipe would otherwise never exit and
/// never be reaped, leaking the runner until `kill_exec`/`terminate`.
async fn pump_exec_lines<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<R>,
    stream: hive_core::LogStream,
    id: String,
    tx: tokio::sync::mpsc::UnboundedSender<hive_core::AgentEvent>,
) {
    let Some(mut reader) = reader else {
        return;
    };
    let mut chunk = [0_u8; 8192];
    let mut pending: Vec<u8> = Vec::with_capacity(4096);
    let mut discard = false;
    let mut emit = |line: &[u8], discard: &mut bool| {
        if *discard {
            return;
        }
        let mut line = line;
        while line.last().is_some_and(|b| *b == b'\r') {
            line = &line[..line.len() - 1];
        }
        let text = if line.len() > EXEC_MAX_LINE_BYTES {
            let mut end = EXEC_MAX_LINE_BYTES;
            while end > 0 && std::str::from_utf8(&line[..end]).is_err() {
                end -= 1;
            }
            format!("{}… [truncated]", String::from_utf8_lossy(&line[..end]))
        } else {
            String::from_utf8_lossy(line).into_owned()
        };
        if tx
            .send(hive_core::AgentEvent::ExecOutput {
                id: id.clone(),
                stream,
                line: text,
            })
            .is_err()
        {
            *discard = true;
        }
    };
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                while let Some(nl) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=nl).collect();
                    emit(&line[..line.len() - 1], &mut discard);
                }
                if pending.len() > EXEC_MAX_LINE_BYTES {
                    let line: Vec<u8> = pending.drain(..).collect();
                    emit(&line, &mut discard);
                }
            }
        }
    }
    if !pending.is_empty() {
        emit(&pending, &mut discard);
    }
}

/// Async half of guest-tar construction for interactive shells AND one-shot
/// execs: the fixed shell/coreutils set plus `extra` programs (the resolved
/// `exec_command` binary), each with its `ldd` closure (a real subprocess
/// spawn — cannot run inside the blocking tar-writer below), merged/deduped
/// and handed to the blocking writer. A closure failure on an EXTRA program
/// (a static binary or a script has none) stages the program alone; the
/// fixed set's closure stays strict, as it always was.
async fn build_guest_tar(archive: File, extra: Vec<PathBuf>) -> anyhow::Result<()> {
    let mut programs: Vec<PathBuf> = SHELL_GUEST_PROGRAMS.iter().map(PathBuf::from).collect();
    let mut deps: Vec<PathBuf> = Vec::new();
    for program in &programs {
        deps.extend(ldd_closure(program).await?);
    }
    for program in SHELL_GUEST_OPTIONAL_PROGRAMS {
        let path = PathBuf::from(program);
        if !path.is_file() {
            tracing::debug!(program, "litebox guest tar: optional tool absent on this host; skipped");
            continue;
        }
        match ldd_closure(&path).await {
            Ok(closure) => {
                deps.extend(closure);
                programs.push(path);
            }
            Err(error) => tracing::debug!(
                program,
                %error,
                "litebox guest tar: optional tool has no resolvable closure; skipped"
            ),
        }
    }
    for program in extra {
        match ldd_closure(&program).await {
            Ok(closure) => deps.extend(closure),
            Err(error) => tracing::debug!(
                program = %program.display(),
                %error,
                "litebox exec: no dynamic closure for program; staging it alone"
            ),
        }
        programs.push(program);
    }
    // Same legacy-soname compat stubs `ensure_combined_tar_locked` stages —
    // a coreutils build linked against a split libpthread/libdl/librt/libutil
    // (rare on a modern glibc host, but the failure mode if it happens is a
    // guest-side dlopen ENOENT with no useful error) gets them for free.
    for stub in [
        "/lib64/libpthread.so.0",
        "/lib64/libdl.so.2",
        "/lib64/librt.so.1",
        "/lib64/libutil.so.1",
    ] {
        let path = PathBuf::from(stub);
        if !deps.contains(&path)
            && std::path::Path::new(stub)
                .metadata()
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
        {
            deps.push(path);
        }
    }
    programs.append(&mut deps);
    programs.sort();
    programs.dedup();
    tokio::task::spawn_blocking(move || build_shell_tar_blocking(archive, &programs))
        .await
        .context("litebox shell guest tar construction task failed")?
}

/// The interactive-shell tar: the fixed set only.
async fn build_shell_tar(archive: File) -> anyhow::Result<()> {
    build_guest_tar(archive, Vec::new()).await
}

#[cfg(target_os = "linux")]
fn build_shell_tar_blocking(archive: File, paths: &[PathBuf]) -> anyhow::Result<()> {
    let root = File::open("/").context("open root filesystem")?;
    let mut seen_inodes: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut builder = tar::Builder::new(archive);
    builder.mode(tar::HeaderMode::Deterministic);
    builder.follow_symlinks(false);

    for path in paths {
        let relative = path
            .strip_prefix("/")
            .map_err(|_| anyhow::anyhow!("shell guest path {} is not absolute", path.display()))?;
        anyhow::ensure!(
            !relative.as_os_str().is_empty(),
            "shell guest path is the root directory"
        );
        let mut file = crate::runtime_artifact::openat2_required(
            &root,
            relative.as_os_str(),
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
            crate::runtime_artifact::RESOLVE_BENEATH
                | crate::runtime_artifact::RESOLVE_NO_MAGICLINKS,
        )
        .with_context(|| format!("openat2 failed for shell guest path {}", path.display()))?;
        let before = file
            .metadata()
            .with_context(|| format!("cannot stat {}", path.display()))?;
        anyhow::ensure!(
            before.is_file(),
            "shell guest path is not a file: {}",
            path.display()
        );
        // Some hosts symlink e.g. /bin -> /usr/bin, or a dependency's
        // resolved path collides with an already-staged program; caught here
        // rather than double-staging the same inode under two tar entries.
        if !seen_inodes.insert((before.dev(), before.ino())) {
            continue;
        }
        let mut header = litebox_tar_header(before.len(), before.mode())?;
        builder
            .append_data(&mut header, relative, &mut file)
            .with_context(|| format!("append shell guest path {}", path.display()))?;
        let after = file.metadata()?;
        anyhow::ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.mode() == after.mode()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec(),
            "shell guest path changed while staging: {}",
            path.display()
        );
    }

    builder
        .finish()
        .context("finish litebox shell guest tar")?;
    let archive = builder
        .into_inner()
        .context("close litebox shell guest tar writer")?;
    archive.sync_all()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn build_shell_tar_blocking(_archive: File, _paths: &[PathBuf]) -> anyhow::Result<()> {
    anyhow::bail!("descriptor-relative tar construction requires Linux openat2")
}

#[async_trait]
impl CellBackend for LiteboxBackend {
    fn name(&self) -> &'static str {
        "litebox"
    }

    fn requires_runtime_artifact_authorization(&self) -> bool {
        true
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
        let mut provision =
            LiteboxProvisionGuard::new(spec.id.clone(), root.clone(), self.cell_nets.clone());
        tokio::fs::create_dir_all(&root).await?;
        if !self.cfg.provision_latency.is_zero() {
            tokio::time::sleep(self.cfg.provision_latency).await;
        }
        // CONTAINER cells bypass litebox's TUN networking entirely (they run
        // via host podman, with podman's own network) — see module doc.
        // Every other cell needs it set up now, before the port is known
        // (mirrors FirecrackerBackend::provision calling setup_cell_net
        // before boot) — a plain function cell with no network can never
        // serve anything, so a setup failure is a hard provision error
        // here, not a silent degrade like Firecracker's egress-only case.
        if spec.container.is_none() {
            anyhow::ensure!(
                self.setup_cell_net(&spec.id).await.is_some(),
                "litebox: failed to set up this cell's TUN device (needs `ip`, CAP_NET_ADMIN, \
                 and /dev/net/tun) — see hive_backend::litebox's module doc, \"Networking\" \
                 section"
            );
        }
        provision.commit();
        Ok(CellHandle {
            id: spec.id.clone(),
            image: spec.image.clone(),
            resources: spec.resources.clone(),
            root,
            endpoint: None,
        })
    }

    async fn provision_runtime(
        &self,
        spec: &CellSpec,
        expected: Option<&RuntimeArtifactIdentity>,
    ) -> anyhow::Result<CellHandle> {
        if spec.container.is_some() {
            anyhow::ensure!(
                expected.is_none(),
                "container launch unexpectedly carried a litebox runtime artifact identity"
            );
        } else {
            let expected = expected.ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: litebox provisioning is missing its caller-authorized runtime artifact identity",
                    hive_core::fault::NODE_IMAGE_MISSING
                )
            })?;
            let attached = self
                .runtime_artifact_identity(&spec.image)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}: litebox runtime artifact is missing for image {}",
                        hive_core::fault::NODE_IMAGE_MISSING,
                        spec.image
                    )
                })?;
            anyhow::ensure!(
                &attached == expected,
                "{}: litebox provisioning observed a different runtime artifact identity",
                hive_core::fault::NODE_IMAGE_MISSING
            );
        }
        self.provision(spec).await
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

    /// Publishes the exact universal package as immutable application authority.
    /// Backend-only runtime closure and identity bytes are added later to a
    /// separately addressed combined archive; no checkout is re-read here.
    async fn deliver_build(
        &self,
        image: &str,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            valid_identity_id(image),
            "runtime artifact image contains an invalid platform identity"
        );
        let publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let package_descriptor = artifact.package_descriptor().clone();
        let app_rel = PathBuf::from(&package_descriptor.app_rel);
        let (staged, _publication) =
            crate::runtime_artifact::stage_sealed_runtime_artifact_serialized(
                artifact,
                directories.staging.descriptor.try_clone()?,
                publication,
            )
            .await?;
        let identity = artifact.identity(image)?;
        anyhow::ensure!(
            staged.content_sha256() == identity.content_sha256,
            "verified runtime package materialized a different semantic identity"
        );
        anyhow::ensure!(
            staged
                .read_regular(
                    Path::new(hive_core::RUNTIME_ARTIFACT_MARKER_FILE),
                    MAX_REFERENCE_BYTES,
                )?
                .is_none(),
            "selected application collides with the platform runtime identity marker"
        );
        let guest_workdir = artifact.guest_workdir(DELIVERED_WORKDIR)?;
        validate_guest_workdir(&guest_workdir)?;
        let (package_scripts, next_entry) = self.package_metadata(&staged, &app_rel).await?;

        let (mut package, verified_descriptor) = artifact.verified_package()?.into_parts();
        anyhow::ensure!(
            verified_descriptor == package_descriptor,
            "sealed universal package descriptor changed during litebox delivery"
        );
        package.seek(SeekFrom::Start(0))?;
        let mut temp = self.allocate_temp_file(&directories.temporary, "app", ".tar")?;
        let copied = {
            let mut input = tokio::fs::File::from_std(package.try_clone()?);
            let mut output = tokio::fs::File::from_std(temp.file.try_clone()?);
            let copied = tokio::io::copy(&mut input, &mut output).await?;
            output.sync_all().await?;
            copied
        };
        anyhow::ensure!(
            copied == package_descriptor.package_bytes,
            "sealed universal package length changed during litebox delivery"
        );
        let app_archive_sha256 = sha256_open_file(&temp.file).await?;
        anyhow::ensure!(
            app_archive_sha256 == package_descriptor.package_sha256,
            "sealed universal package bytes changed during litebox delivery"
        );
        let destination_name = Self::app_archive_name(&app_archive_sha256);
        self.publish_immutable_locked(
            &directories,
            &mut temp,
            &directories.apps,
            &destination_name,
            &app_archive_sha256,
        )
        .await?;
        let reference = LiteboxImageReference {
            schema: LITEBOX_ARTIFACT_SCHEMA,
            image: image.to_string(),
            identity,
            guest_workdir,
            app_archive_sha256,
            package_scripts,
            next_entry,
            runtimes: BTreeMap::new(),
        };
        self.write_image_reference_locked(&directories, &reference)
            .await?;
        if let Err(error) = self.gc_artifacts_locked(&directories).await {
            tracing::warn!(error = %error, "litebox artifact GC refused or failed");
        }
        Ok(())
    }

    async fn runtime_artifact_identity(
        &self,
        image: &str,
    ) -> anyhow::Result<Option<RuntimeArtifactIdentity>> {
        anyhow::ensure!(
            valid_identity_id(image),
            "runtime artifact image contains an invalid platform identity"
        );
        let _publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let name = Self::image_reference_name(image);
        let file = match directories.references.open_regular(&name) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let bytes = read_bounded_file(file, MAX_REFERENCE_BYTES)?;
        let reference: LiteboxImageReference = serde_json::from_slice(&bytes)?;
        validate_image_reference(&reference, image)?;
        verify_immutable_open(
            &directories.apps,
            &Self::app_archive_name(&reference.app_archive_sha256),
            &reference.app_archive_sha256,
        )
        .await?;
        directories.verify_bindings()?;
        Ok(Some(reference.identity))
    }

    fn delivered_workdir(
        &self,
        artifact: &SealedRuntimeArtifact,
    ) -> anyhow::Result<Option<String>> {
        artifact.guest_workdir(DELIVERED_WORKDIR).map(Some)
    }

    async fn start_function(
        &self,
        cell: &CellHandle,
        func: &FunctionLaunch,
    ) -> anyhow::Result<CellEndpoint> {
        if func.start_cmd.is_empty() {
            return Err(launch_refusal("function start command is empty"));
        }

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
            // Extra raw/published TCP publishes (`FunctionLaunch::tcp_ports` —
            // includes the primary, whose pairing is already ports[0]; a
            // duplicate `-p` for the same pair fails the run). Without these
            // the raw proxy's per-port loopback leg (`Lease::tcp_host_port`)
            // dials a port nothing publishes — connection refused on every
            // node running THIS backend while the mock path worked.
            ports.extend(func.tcp_ports.iter().filter_map(|t| {
                if t.host_port == func.port {
                    return None;
                }
                Some(crate::ContainerPort {
                    container_port: t.container_port,
                    host_port: t.host_port,
                    protocol: crate::ContainerProtocol::Tcp,
                })
            }));
            let launch = crate::podman_run_container(
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
            let endpoint = launch.endpoint();
            self.containers.lock().await.insert(cell.id.clone(), launch);
            return Ok(endpoint);
        }

        // Bun is refused here, before any reservation, network setup, tar
        // alias, or process spawn — never after. `func.runtime` (the
        // authoritative platform discriminator FunctionLaunch itself carries,
        // never a parse of `start_cmd`'s tenant-controlled argv text) is the
        // only signal trusted for this decision, exactly like
        // `hive-cell-agent`'s own `platform_runtime_program` gate.
        //
        // This backend's syscall shim panics (`unimplemented!()`,
        // `litebox_shim_linux/src/syscalls/file.rs:1210`) on Bun's own
        // boot-time `readlink("/proc/self/fd/3")` probe — a hard Rust panic
        // inside the guest's interception layer, not a recoverable error.
        // Letting a Bun launch reach `resolve_direct_launch`/spawn below would
        // turn every Bun cold start into an uncontrolled crash instead of an
        // honest, typed node fault. This mirrors `resources::detect`'s
        // `bun_runtime: Some(false)` verdict for this backend — placement
        // should already have excluded this node for a Bun deployment
        // (`schedule::bun_capable`), so reaching here means the gossiped
        // capability and this launch disagree (a stale registry entry, a
        // launch forced past placement). Never falls back to Mock or the
        // host, never classifies as capacity or a tenant-app fault — this is
        // a NODE fault, the same class `NODE_IMAGE_MISSING` and
        // `NODE_BACKEND_UNAVAILABLE` are.
        if func.runtime == hive_core::Runtime::Bun {
            return Err(bun_unsupported_refusal());
        }

        // Plain function: bind this cell to the exact durable reference before
        // resolving any repository-controlled launch metadata. Package managers
        // are never run here: a narrow, validated package-script reducer emits
        // one direct Node process or a typed backend-capability refusal (Bun is
        // refused above, before this lock, before any of the machinery below).
        let _publication = self.artifact_lock.clone().lock_owned().await;
        let directories = self.prepare_artifact_dirs()?;
        let mut reference = self
            .load_image_reference_locked(&directories, &cell.image)
            .await?;
        let runtime_workdir = func.workdir.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "{}: litebox function is missing its exact runtime artifact workdir",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        anyhow::ensure!(
            runtime_workdir == reference.guest_workdir,
            "{}: litebox runtime workdir mismatch (launch {:?}, artifact {:?})",
            hive_core::fault::NODE_IMAGE_MISSING,
            runtime_workdir,
            reference.guest_workdir
        );
        let expected = func.runtime_artifact.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "{}: litebox launch is missing its authoritative runtime artifact identity",
                hive_core::fault::NODE_IMAGE_MISSING
            )
        })?;
        anyhow::ensure!(
            expected == &reference.identity,
            "{}: litebox launch requested a different runtime artifact identity",
            hive_core::fault::NODE_IMAGE_MISSING
        );
        // `Runtime::Bun` is refused unconditionally above, before this branch
        // is ever reached, so only `Node` remains a legitimate plain-function
        // runtime on this backend.
        if func.runtime != hive_core::Runtime::Node {
            return Err(launch_refusal(format!(
                "runtime {:?} is not a Node runtime",
                func.runtime
            )));
        }
        let app_archive = verify_immutable_open(
            &directories.apps,
            &Self::app_archive_name(&reference.app_archive_sha256),
            &reference.app_archive_sha256,
        )
        .await?;
        let DirectLaunch {
            runtime,
            bin,
            mut args,
        } = resolve_direct_launch(func, &reference, &app_archive).await?;
        // Belt-and-braces for the same panic this function's entry already
        // refuses on `func.runtime`: `resolve_direct_launch` derives ITS
        // runtime from `start_cmd`'s argv text (needed to tell `node` from
        // `bun` from a shared package-script reducer), so a launch whose
        // `start_cmd` names `bun` while `func.runtime` claims something else
        // — a malformed or mismatched launch that reached this point despite
        // the entry check — would otherwise still be spawned against the
        // unsupported syscall shim. The entry check above already refused
        // every launch where `func.runtime` itself says Bun, so reaching
        // `DirectRuntime::Bun` here can only mean that disagreement.
        if runtime == DirectRuntime::Bun {
            return Err(bun_unsupported_refusal());
        }
        anyhow::ensure!(
            bin.is_file(),
            "{}: `{}` is not installed on this node, so the direct runtime cannot start here (operator remedy; not an application fault)",
            hive_core::fault::NODE_RUNTIME_MISSING,
            bin.display()
        );
        let runtime_key = format!(
            "{}:{}",
            match runtime {
                DirectRuntime::Node => "node",
                DirectRuntime::Bun => "bun",
            },
            bin.display()
        );
        let initial_files = self
            .ensure_combined_tar_locked(
                &directories,
                &cell.image,
                &runtime_key,
                &bin,
                &mut reference,
            )
            .await?;

        let net = self
            .cell_nets
            .lock()
            .await
            .get(&cell.id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "litebox: cell {} has no TUN device (provision should have set one up) — this is a bug, not a runtime condition",
                    cell.id
                )
            })?;
        if runtime == DirectRuntime::Bun {
            args.insert(0, GUEST_RUNTIME_GUARD_PATH.to_string());
            args.insert(0, "--preload".to_string());
        }

        let guest_environment = sanitized_guest_environment(func)?;
        let mut command = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut command);
        let initial_files_alias =
            self.allocate_initial_files_alias(&directories.temporary, &initial_files)?;
        let initial_files_path = crate::runtime_artifact::inherit_file_path(
            &mut command,
            &initial_files_alias.directory,
        )?
        .join(INITIAL_FILES_ALIAS_NAME);
        directories.verify_bindings()?;
        command
            .arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
            .arg(format!("--initial-files={}", initial_files_path.display()))
            .arg("--")
            .arg(&bin)
            .args(&args)
            // Nothing from hive-node is inherited. Host-loader, runner-control
            // and platform-owned keys were removed before the remaining tenant
            // application environment was inserted.
            .env_clear()
            .envs(&guest_environment)
            .env("PORT", func.port.to_string())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/")
            .env("LITEBOX_GUEST_IP", &net.guest_ip)
            .env("LITEBOX_GATEWAY_IP", &net.host_ip)
            .env("HIVE_RUNTIME_WORKDIR", &reference.guest_workdir)
            .env(
                "HIVE_RUNTIME_ARTIFACT_PROTOCOL",
                reference.identity.protocol.to_string(),
            )
            .env("HIVE_RUNTIME_ARTIFACT_ID", &reference.identity.id)
            .env(
                "HIVE_RUNTIME_ARTIFACT_SHA256",
                &reference.identity.content_sha256,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Piped, not nulled: a runtime that exits before listening is
            // reported to the tenant/operator only through the launch error, so
            // a discarded stderr turned every guest-side failure (shim panic,
            // module resolution, bind refusal) into an unexplained "exit
            // status: 1" — witnessed live on nodes-wtf. A bounded tail of it is
            // folded into that error; a HEALTHY guest's stderr keeps draining
            // into the same bounded buffer so the pipe can never fill and
            // backpressure the app.
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if runtime == DirectRuntime::Node {
            command.env(
                "NODE_OPTIONS",
                format!("--require {GUEST_RUNTIME_GUARD_PATH}"),
            );
        } else {
            // Bun's CLI preload above is the ordered guard. Do not let a
            // tenant-supplied compatibility/options variable inject an earlier
            // preload ahead of that platform-owned handshake.
            command.env_remove("NODE_OPTIONS").env_remove("BUN_OPTIONS");
        }

        let mut child = command.spawn().map_err(|error| {
            anyhow::anyhow!(
                "failed to spawn litebox runner at {}: {error}",
                self.cfg.runner_bin.display()
            )
        })?;
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        if let Some(mut pipe) = child.stderr.take() {
            const STDERR_TAIL_CAP: usize = 8 * 1024;
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buffer = [0_u8; 4096];
                while let Ok(read) = pipe.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    tail.extend_from_slice(&buffer[..read]);
                    if tail.len() > STDERR_TAIL_CAP {
                        let excess = tail.len() - STDERR_TAIL_CAP;
                        tail.drain(..excess);
                    }
                }
            });
        }
        let func_addr = format!("{}:{}", net.guest_ip, func.port);
        // Armed for the whole readiness wait: covers a normal timeout/exit
        // `Err` AND the future being dropped outright (the caller gave up —
        // see `ChildReapGuard`'s doc). Disarmed only once the guest has
        // proven it is listening, at which point `child` moves into
        // long-lived tracking and `terminate()` becomes the reaper.
        let mut reap_guard = ChildReapGuard::new(&mut child);
        if let Err(error) =
            wait_litebox_ready(reap_guard.child_mut(), &func_addr, Duration::from_secs(15)).await
        {
            // Give the reader a beat to drain what the dying guest wrote.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let tail = {
                let tail = stderr_tail
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                String::from_utf8_lossy(&tail).trim().to_string()
            };
            if tail.is_empty() {
                return Err(error);
            }
            return Err(anyhow::anyhow!("{error}; guest stderr tail: {tail}"));
        }
        reap_guard.disarm();
        // Guest readiness proves Litebox has materialized the initial tar. Drop
        // the descriptor-relative pathname now; cancellation before this point
        // takes the same Drop cleanup path automatically.
        drop(initial_files_alias);
        // Keep publication/GC serialized until the runner has consumed the
        // selected immutable archive. A concurrent redelivery can then retire
        // the old reference without deleting the file from under this cold
        // start; a ready guest has already materialized its in-memory fs.
        drop(_publication);

        let raw_proxy = func.raw_proxy;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let tunnel_addr = listener.local_addr()?.to_string();
        let max_conc = func.max_concurrency.max(1);
        let task = crate::AbortTask::new(tokio::spawn(async move {
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
        }));
        let mut funcs = self.funcs.lock().await;
        let mut tunnels = self.tunnels.lock().await;
        funcs.insert(
            cell.id.clone(),
            LiteboxFunctionProcess {
                child,
                _initial_files: initial_files,
            },
        );
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
        let cell_nets = self.cell_nets.clone();
        let execs = self.execs.clone();
        let cleanup = tokio::spawn(async move {
            let tunnel = tunnels.lock().await.remove(&id);
            if let Some(task) = tunnel {
                task.abort();
            }
            let container = containers.lock().await.remove(&id);
            if let Some(container) = container {
                container.terminate().await;
            }
            // Every live sandbox exec in this cell — one-shot commands AND
            // interactive shells, each its own process group — dies with the
            // cell, before its TUN does. A guest that ignores its stdio (a
            // spinning fork child, `sleep infinity`) otherwise outlives the
            // sandbox that owned it: witnessed, a DELETE answered 200 while
            // the runner held a core at 100% until an operator killed it.
            let doomed: Vec<(String, i32)> = {
                let mut execs = execs.lock().await;
                let ids: Vec<String> = execs
                    .iter()
                    .filter(|(_, e)| e.cell == id)
                    .map(|(k, _)| k.clone())
                    .collect();
                ids.into_iter()
                    .filter_map(|k| execs.remove(&k).map(|e| (k, e.pgid)))
                    .collect()
            };
            for (exec_id, pgid) in doomed {
                #[cfg(unix)]
                {
                    // SAFETY: killpg(2) with a pgid this process spawned and a
                    // signal number; no pointer arguments.
                    let rc = unsafe { libc::killpg(pgid, libc::SIGKILL) };
                    if rc != 0 {
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            tracing::warn!(
                                cell = %id,
                                exec = %exec_id,
                                pgid,
                                %error,
                                "litebox terminate: killpg on a live exec failed"
                            );
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = (exec_id, pgid);
                }
            }
            let process = funcs.lock().await.remove(&id);
            if let Some(mut process) = process {
                let _ = process.child.start_kill();
                let _ = process.child.wait().await;
            }
            let net = cell_nets.lock().await.remove(&id);
            if let Some(net) = net {
                delete_litebox_link(&net.tun_dev).await;
            }
            let _ = tokio::fs::remove_dir_all(root).await;
        });
        cleanup
            .await
            .map_err(|e| anyhow::anyhow!("litebox cleanup task failed: {e}"))
    }

    async fn cpu_percent(&self, cell: &CellHandle) -> Option<f32> {
        // Guest and runner are one process (litebox has no separate VMM), so
        // the runner's own PID directly IS the guest's CPU usage — sampling
        // is more direct here than the Firecracker VMM-proxy case.
        let pid = {
            let funcs = self.funcs.lock().await;
            funcs.get(&cell.id).and_then(|c| c.child.id())?
        };
        self.sampler.cpu_percent(pid, cell.resources.vcpus)
    }

    /// One argv command inside `cell` (Sandboxes `run_command`) — the SAME
    /// guest mechanism `exec_pty` uses: a fresh `litebox-runner` spawned
    /// against the cell's TUN device with a guest tar holding the fixed
    /// shell/coreutils set plus the resolved program and its `ldd` closure.
    /// stdout/stderr are separate pipes streamed as distinct `ExecOutput`
    /// lines, then exactly one `ExecDone { exit_code }` — `None` when the
    /// runner died by signal (a `kill_exec`), never a fake `Some(0)`. The
    /// runner is its own process-group leader so `kill_exec` can terminate
    /// the whole guest tree by exec id. Returns as soon as the runner is
    /// spawned; the caller drains the channel.
    async fn exec_command(
        &self,
        cell: &CellHandle,
        req: hive_core::ExecRequest,
    ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>> {
        anyhow::ensure!(
            !req.id.is_empty(),
            "litebox exec requires a non-empty exec id"
        );
        // The runner is an unprivileged host process; there is no `sudo` in
        // the guest and no privilege to elevate to. Refuse loudly at start.
        anyhow::ensure!(
            !req.sudo,
            "litebox sandboxes cannot elevate privileges: sudo is not available inside a litebox guest"
        );
        if self.execs.lock().await.contains_key(&req.id) {
            anyhow::bail!("exec id {} is already running on this node", req.id);
        }
        let net = self
            .cell_nets
            .lock()
            .await
            .get(&cell.id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "litebox: cell {} has no TUN device (provision should have set one up) — this is a bug, not a runtime condition",
                    cell.id
                )
            })?;

        let (program, args) = if req.shell {
            let mut line = req.cmd.clone();
            for arg in &req.args {
                line.push(' ');
                line.push_str(arg);
            }
            (PathBuf::from("/bin/sh"), vec!["-c".to_string(), line])
        } else {
            (resolve_guest_program(&req.cmd)?, req.args.clone())
        };

        let directories = self.prepare_artifact_dirs()?;
        directories.verify_bindings()?;
        let exec_tar = self.allocate_temp_file(&directories.temporary, "exec", ".tar")?;
        build_guest_tar(exec_tar.file.try_clone()?, vec![program.clone()]).await?;
        let initial_files_alias =
            self.allocate_initial_files_alias(&directories.temporary, &exec_tar.file)?;

        let cwd = if req.cwd.is_empty() {
            "/".to_string()
        } else {
            req.cwd.clone()
        };
        let mut command = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut command);
        let initial_files_path = crate::runtime_artifact::inherit_file_path(
            &mut command,
            &initial_files_alias.directory,
        )?
        .join(INITIAL_FILES_ALIAS_NAME);
        command
            .arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
            .arg(format!("--initial-files={}", initial_files_path.display()))
            .arg("--")
            .arg(&program)
            .args(&args)
            .env_clear()
            .envs(&req.env)
            .env("HOME", "/root")
            .env("PATH", "/usr/bin:/bin")
            .env("LITEBOX_GUEST_IP", &net.guest_ip)
            .env("LITEBOX_GATEWAY_IP", &net.host_ip)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group: `kill_exec` sends SIGKILL to the GROUP, which
        // reaches every process the guest forked, not only the runner.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|error| {
            anyhow::anyhow!(
                "failed to spawn litebox runner for sandbox exec at {}: {error}",
                self.cfg.runner_bin.display()
            )
        })?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("litebox runner exited before its pid could be read"))?;
        let pgid = i32::try_from(pid).context("litebox runner pid does not fit a pgid")?;
        self.execs.lock().await.insert(
            req.id.clone(),
            LiteboxExec {
                cell: cell.id.clone(),
                pgid,
            },
        );

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stdout = tokio::spawn(pump_exec_lines(
            child.stdout.take(),
            hive_core::LogStream::Stdout,
            req.id.clone(),
            tx.clone(),
        ));
        let stderr = tokio::spawn(pump_exec_lines(
            child.stderr.take(),
            hive_core::LogStream::Stderr,
            req.id.clone(),
            tx.clone(),
        ));
        let execs = self.execs.clone();
        let exec_id = req.id.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            // Every output line precedes the terminal event.
            let _ = stdout.await;
            let _ = stderr.await;
            execs.lock().await.remove(&exec_id);
            // `code()` is `None` for a signal death — exactly the "killed"
            // meaning the protocol reserves for `exit_code: None`.
            let exit_code = status.ok().and_then(|s| s.code());
            let _ = tx.send(hive_core::AgentEvent::ExecDone {
                id: exec_id,
                exit_code,
            });
            // The runner has exited, so the guest tar and its private alias
            // are provably no longer being read; their Drop guards unlink.
            drop(initial_files_alias);
            drop(exec_tar);
        });
        Ok(rx)
    }

    /// Terminate a still-running `exec_command` by id: SIGKILL to the whole
    /// process group the runner leads (the guest's forked descendants
    /// included). Idempotent — an id that already finished is a no-op. The
    /// waiter task then observes the signal death and emits
    /// `ExecDone { exit_code: None }`.
    async fn kill_exec(&self, cell: &CellHandle, exec_id: &str) -> anyhow::Result<()> {
        let entry = self.execs.lock().await.get(exec_id).cloned();
        let Some(entry) = entry else {
            return Ok(());
        };
        anyhow::ensure!(
            entry.cell == cell.id,
            "exec {exec_id} belongs to cell {}, not {}",
            entry.cell,
            cell.id
        );
        #[cfg(unix)]
        {
            // SAFETY: killpg(2) with a pgid this process spawned and a signal
            // number; no pointer arguments.
            let rc = unsafe { libc::killpg(entry.pgid, libc::SIGKILL) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                // ESRCH: the group is already gone (raced its own exit) —
                // the waiter reports the real exit either way.
                anyhow::ensure!(
                    error.raw_os_error() == Some(libc::ESRCH),
                    "killpg({}) for exec {exec_id} failed: {error}",
                    entry.pgid
                );
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("litebox exec kill requires a Unix process group")
        }
    }

    async fn exec_pty(
        &self,
        cell: &CellHandle,
        req: hive_core::ExecPtyRequest,
    ) -> anyhow::Result<(
        tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>,
        crate::PtyIo,
    )> {
        // litebox has no separate guest kernel/agent — the guest IS this
        // host's own `litebox-runner` child process (syscall interception,
        // not virtualization), so unlike Firecracker's vsock-agent-protocol
        // exec_pty, no wire protocol is needed at all: a REAL host pty
        // allocated here and handed to the runner as its stdin/stdout/stderr
        // gives the guest shell genuine raw-mode terminal behavior for free
        // (the `AnEntrypoint/litebox` base this platform tracks also
        // implements its own guest-internal /dev/ptmx pty subsystem — see
        // ansible/roles/litebox/files/PATCHES.md's "litebox fork tracking"
        // section — but wiring THIS host-side pty is simpler and needs none
        // of that: the shell's own line discipline runs against a real
        // kernel pty exactly as it would over SSH, the same technique
        // `hive-cell-agent`'s Firecracker-guest PTY support uses on the
        // OTHER side of the vsock boundary).
        let net = self
            .cell_nets
            .lock()
            .await
            .get(&cell.id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "litebox: cell {} has no TUN device (provision should have set one up) — this is a bug, not a runtime condition",
                    cell.id
                )
            })?;

        let directories = self.prepare_artifact_dirs()?;
        directories.verify_bindings()?;
        let shell_tar = self.allocate_temp_file(&directories.temporary, "shell", ".tar")?;
        build_shell_tar(shell_tar.file.try_clone()?).await?;
        let initial_files_alias =
            self.allocate_initial_files_alias(&directories.temporary, &shell_tar.file)?;

        let (master_fd, slave_fd) = open_pty_pair(req.cols, req.rows)
            .context("litebox: failed to allocate a host pty for the sandbox shell")?;

        let shell = if req.shell.is_empty() {
            "/bin/sh".to_string()
        } else {
            req.shell.clone()
        };
        let cwd = if req.cwd.is_empty() {
            "/".to_string()
        } else {
            req.cwd.clone()
        };

        let mut command = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut command);
        let initial_files_path = crate::runtime_artifact::inherit_file_path(
            &mut command,
            &initial_files_alias.directory,
        )?
        .join(INITIAL_FILES_ALIAS_NAME);
        command
            .arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
            .arg(format!("--initial-files={}", initial_files_path.display()))
            .arg("--")
            .arg(&shell)
            .env_clear()
            .envs(&req.env)
            .env("HOME", "/root")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("LITEBOX_GUEST_IP", &net.guest_ip)
            .env("LITEBOX_GATEWAY_IP", &net.host_ip)
            .current_dir(&cwd)
            // SAFETY: dup'd fds are valid, open, and owned until this Stdio
            // takes them — the runner inherits them across exec as its own
            // fd 0/1/2, exactly the real-terminal shape a login shell expects.
            .stdin(Stdio::from(slave_fd.try_clone()?))
            .stdout(Stdio::from(slave_fd.try_clone()?))
            .stderr(Stdio::from(slave_fd))
            .kill_on_drop(true);
        // Own process group, tracked in `execs` under the session id exactly
        // like a one-shot exec, so `terminate` can kill a shell (and whatever
        // it forked) together with its cell. The runner already has no
        // controlling terminal (no setsid/TIOCSCTTY), so job control is
        // unchanged by the setpgid.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|error| {
            anyhow::anyhow!(
                "failed to spawn litebox runner for interactive shell at {}: {error}",
                self.cfg.runner_bin.display()
            )
        })?;
        let session_id = req.id.clone();
        let cell_id = cell.id.clone();
        let funcs = self.funcs.clone();
        let execs = self.execs.clone();
        if let Some(pid) = child.id() {
            if let Ok(pgid) = i32::try_from(pid) {
                execs.lock().await.insert(
                    session_id.clone(),
                    LiteboxExec {
                        cell: cell_id.clone(),
                        pgid,
                    },
                );
            }
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut master_read = tokio::fs::File::from_std(master_fd.try_clone()?.into());
        let tx_reader = tx.clone();
        let id_reader = session_id.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buffer = [0_u8; 8192];
            loop {
                match master_read.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx_reader
                            .send(hive_core::AgentEvent::PtyOutput {
                                id: id_reader.clone(),
                                bytes: buffer[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    // EIO on a pty master is the normal "slave closed" signal
                    // once the shell (and everything else holding the slave
                    // open) has exited — not a real error.
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(_) => break,
                }
            }
        });

        let id_waiter = session_id.clone();
        tokio::spawn(async move {
            let status = child.wait().await.ok();
            execs.lock().await.remove(&id_waiter);
            let exit_code = status.and_then(|s| s.code());
            let _ = tx.send(hive_core::AgentEvent::PtyExited {
                id: id_waiter,
                exit_code,
            });
            // Drop this cell's placeholder from `funcs` if a real function
            // launch never claimed the slot — best-effort, the interactive
            // shell was never registered there to begin with, this just
            // guards a hypothetical future caller that keys off cell_id.
            let _ = funcs.lock().await.remove(&cell_id);
            // The guest tar and its descriptor-bound alias MUST outlive the
            // runner: their Drop guards unlink the alias name and its scratch
            // directory. Dropping them at the end of `exec_pty` (as this
            // function once did) raced the runner's own open of
            // `--initial-files=/proc/self/fd/N/initial-files.tar` — the entry
            // was gone a few milliseconds after spawn, litebox died with a bare
            // `No such file or directory (os error 2)` on the pty, and every
            // dashboard terminal "instantly disconnected" (witnessed
            // 2026-09-01 on fc-sanjose: 101 upgrade, then that error frame and
            // `{"type":"exited","exit_code":1}`). `exec_command` has always
            // held both in its waiter; this is the same discipline.
            drop(initial_files_alias);
            drop(shell_tar);
        });

        let master_for_io = std::sync::Arc::new(master_fd);
        let master_input = master_for_io.clone();
        let master_resize = master_for_io;
        let pty = crate::PtyIo::new(
            move |bytes: Vec<u8>| {
                use std::io::Write;
                let mut f = &*master_input;
                let _ = f.write_all(&bytes);
            },
            move |cols: u16, rows: u16| {
                let ws = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                unsafe {
                    libc::ioctl(
                        std::os::fd::AsRawFd::as_raw_fd(&*master_resize),
                        libc::TIOCSWINSZ as _,
                        &ws,
                    );
                }
            },
        );
        Ok((rx, pty))
    }
}

/// Allocate a real host pty (`openpty(3)`) sized to `cols`x`rows`, returned as
/// a `(master, slave)` pair of owned fds. Used by `LiteboxBackend::exec_pty`
/// to give an interactive sandbox shell genuine raw-mode terminal behavior —
/// see that method's own doc comment for why a host-side pty is sufficient
/// here (litebox's guest and this host process share one address space, so
/// handing the slave straight to the spawned runner as its stdin/stdout/
/// stderr works exactly like a real terminal session, no in-guest pty
/// subsystem required).
#[cfg(target_os = "linux")]
fn open_pty_pair(cols: u16, rows: u16) -> anyhow::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::FromRawFd;
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: openpty is given valid out-params and a stack winsize; on
    // success both fds are open, valid, and owned by this process.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    };
    anyhow::ensure!(rc == 0, "openpty failed: {}", std::io::Error::last_os_error());
    // SAFETY: both fds were just returned by a successful openpty() call
    // above and are not owned anywhere else yet.
    let master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    Ok((master, slave))
}

#[cfg(not(target_os = "linux"))]
fn open_pty_pair(_cols: u16, _rows: u16) -> anyhow::Result<(std::fs::File, std::fs::File)> {
    anyhow::bail!("litebox interactive shell requires a real Linux pty (openpty)")
}
