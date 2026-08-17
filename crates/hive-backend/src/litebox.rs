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
//! their absolute paths and (b) the deployment's own file tree at paths
//! relative to its own root — the guest's default cwd IS that root, so
//! `node server.js` plus `require()` of local files/`node_modules` resolves
//! exactly like a host process rooted at the build dir would, no path
//! rewriting needed. [`LiteboxBackend::deliver_build`]/`start_function`
//! implement this; there is no compile-cache directory across cold starts
//! for the same reason (the guest fs is a fresh in-memory snapshot every
//! process, nothing persists back to the host).
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
            // `root` is per-cell RUNTIME scratch, recreated on every provision
            // and removed on terminate — temp is correct for it.
            root: std::env::temp_dir().join("hive-litebox-cells"),
            // `cache_root` is NOT scratch: it holds the DELIVERED build tar,
            // the one artifact `start_function` cannot run without
            // (`ensure_combined_tar` bails when it is missing). Under
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

impl LiteboxBackend {
    pub fn new(cfg: LiteboxConfig) -> Self {
        LiteboxBackend {
            cfg,
            funcs: Arc::new(AsyncMutex::new(HashMap::new())),
            tunnels: Arc::new(AsyncMutex::new(HashMap::new())),
            containers: Arc::new(AsyncMutex::new(HashMap::new())),
            ctnl_tasks: Arc::new(AsyncMutex::new(HashMap::new())),
            cell_nets: Arc::new(AsyncMutex::new(HashMap::new())),
            net_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            sampler: Arc::new(crate::CpuSampler::new()),
        }
    }

    /// HOST scratch path the embedded shim's content is written to before
    /// being tar'd into a guest-visible tar by `stage_bind_shim` — never
    /// referenced by `NODE_OPTIONS` directly, see [`GUEST_BIND_SHIM_PATH`].
    fn bind_shim_host_scratch_path(&self) -> PathBuf {
        self.cfg.root.join("hive-litebox-bind-shim.js")
    }

    /// Write the embedded shim to the host scratch path, then append it
    /// into `tar_path` at [`GUEST_BIND_SHIM_PATH`]'s bare filename so it
    /// becomes visible to the guest at that exact absolute path.
    async fn stage_bind_shim(&self, tar_path: &Path) -> anyhow::Result<()> {
        let host_path = self.bind_shim_host_scratch_path();
        if let Some(parent) = host_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(&host_path, NODE_BIND_SHIM_JS).await?;
        let dir = host_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("bind shim scratch path has no parent"))?;
        let name = host_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("bind shim scratch path has no filename"))?;
        let out = Command::new("tar")
            .arg("-C")
            .arg(dir)
            .arg("-rf")
            .arg(tar_path)
            .arg(name)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run tar: {e}"))?;
        anyhow::ensure!(
            out.status.success(),
            "tar failed staging the bind shim into {}: {}",
            tar_path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(())
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
            let _ = Command::new("/bin/sh").arg("-c").arg(&script).status().await;
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
        // Carries `NODE_IMAGE_MISSING` for the same reason the Firecracker path
        // does: this is a per-node ARTIFACT that should be here and is not, so
        // the remedy is to reprovision/redeliver on this node — not to read the
        // app's logs, and not to add capacity. Without a marker every fault this
        // backend raises falls into `classify_lease_error`'s catch-all and is
        // published as CAPACITY_EXHAUSTED, which on fc-frankfurt (the one node
        // serving real tenant traffic on litebox) means every backend failure
        // there currently blames the host for having no room.
        anyhow::ensure!(
            app_tar.exists(),
            "{}: litebox: no delivered build staged for image {image} — deliver_build must run \
             before start_function (app tar missing at {})",
            hive_core::fault::NODE_IMAGE_MISSING,
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
        // Stage the bind-rewrite shim into every cell's tar unconditionally
        // (cheap — one small JS file) rather than trying to detect whether
        // this deployment's runtime will actually use it.
        self.stage_bind_shim(&tmp).await?;
        tokio::fs::rename(&tmp, &combined).await?;
        Ok(combined)
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
        let tar_path = self.cfg.root.join("litebox-network-smoke-deps.tar");
        if let Some(parent) = tar_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        // Always build the tar (never conditional on `deps` being non-empty
        // — it always needs at least the bind shim) and always pass
        // `--initial-files`, so the guest can find it.
        let out = Command::new("tar")
            .arg("-h")
            .arg("--absolute-names")
            .arg("-cf")
            .arg(&tar_path)
            .args(&deps)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run tar: {e}"))?;
        anyhow::ensure!(
            out.status.success(),
            "tar failed staging node's deps for the network smoke test: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        self.stage_bind_shim(&tar_path).await?;

        let marker = format!("litebox-net-smoke-{}", now_ms());
        // A wildcard bind (`.listen(PROBE_PORT)`, no host) is the exact case
        // `networking.patch` fixes natively — this proves the patch, not
        // just the TUN plumbing around it. The bind-rewrite shim is ALSO
        // preloaded (harmless no-op for this wildcard case, but proves the
        // shim mechanism itself didn't break — see `LiteboxBackend`'s
        // module doc, "Networking").
        let script =
            format!("require('http').createServer((q,r)=>{{r.end('{marker}')}}).listen({PROBE_PORT});");

        let mut cmd = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut cmd);
        cmd.arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
            .arg(format!("--initial-files={}", tar_path.display()))
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
        // The interpreter must exist on the HOST here — this backend stages the
        // binary's own ldd closure into the guest tar, so a name that resolves
        // to nothing produces a confusing staging failure rather than an
        // actionable one. Same preflight and same marker as the mock and
        // cell-agent paths, so a missing runtime is reported as an operator
        // remedy instead of falling into the CAPACITY_EXHAUSTED catch-all.
        anyhow::ensure!(
            bin.is_file(),
            "{}: `{}` is not installed on this node, so a runtime=\"{}\" deployment \
             cannot start here (operator remedy; not an application fault)",
            hive_core::fault::NODE_RUNTIME_MISSING,
            func.start_cmd[0],
            func.runtime.as_str(),
        );
        let initial_files = self.ensure_combined_tar(&cell.image, &bin).await?;

        // This cell's TUN device + real, distinct guest IP, set up in
        // `provision` — see module doc's "Networking" section (litebox's
        // guest IP is patched to be per-invocation configurable, so no
        // namespace/veth isolation is needed — each cell just gets its own
        // real address directly).
        let net = self
            .cell_nets
            .lock()
            .await
            .get(&cell.id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "litebox: cell {} has no TUN device (provision should have set one up) — \
                     this is a bug, not a runtime condition",
                    cell.id
                )
            })?;

        // The bind-rewrite shim (see module doc's "Networking" section +
        // litebox-bind-shim.js's own doc comment) is already staged into
        // `initial_files` by `ensure_combined_tar` -> `stage_bind_shim`;
        // only Node/Bun actually preload it. Even with the wildcard-bind
        // patch applied, an app that explicitly hardcodes a loopback
        // address still needs rewriting to this cell's real guest IP — TUN
        // can never bridge host<->guest loopback. Python is not covered
        // yet — see `crate::litebox`'s tracking note.
        let is_node = matches!(func.runtime, hive_core::Runtime::Node | hive_core::Runtime::Bun);

        let mut cmd = Command::new(&self.cfg.runner_bin);
        reset_signal_dispositions_before_exec(&mut cmd);
        cmd.arg("-Z")
            .arg("--rewrite-syscalls")
            .arg("--forward-env")
            .arg(format!("--tun-device-name={}", net.tun_dev))
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
            // Read by ansible/roles/litebox/files/networking.patch's
            // litebox_runner_linux_userland change — gives THIS cell its own
            // real, distinct guest identity instead of every cell defaulting
            // to the same hardcoded 10.0.0.2/10.0.0.1.
            .env("LITEBOX_GUEST_IP", &net.guest_ip)
            .env("LITEBOX_GATEWAY_IP", &net.host_ip)
            .envs(&func.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if is_node {
            cmd.env("NODE_OPTIONS", format!("--require {GUEST_BIND_SHIM_PATH}"));
        }

        let child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn litebox runner at {}: {e}",
                self.cfg.runner_bin.display()
            )
        })?;
        self.funcs.lock().await.insert(cell.id.clone(), child);

        let func_addr = format!("{}:{}", net.guest_ip, func.port);
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
        // Bind each `remove` out of its guard before the following await — see
        // `firecracker.rs::terminate` for the full reasoning (an `if let` over
        // `map.lock().await.remove(..)` holds the map for the whole body, i.e.
        // across `podman_stop_container` and `child.wait()`, blocking
        // `cpu_percent` and cold starts node-wide during a drain).
        let tunnel = self.tunnels.lock().await.remove(&cell.id);
        if let Some(task) = tunnel {
            task.abort();
        }
        let ctnl_task = self.ctnl_tasks.lock().await.remove(&cell.id);
        if let Some(task) = ctnl_task {
            task.abort();
        }
        let container = self.containers.lock().await.remove(&cell.id);
        if let Some(name) = container {
            crate::podman_stop_container(&name, crate::PODMAN_PATH).await;
        }
        let func = self.funcs.lock().await.remove(&cell.id);
        if let Some(mut child) = func {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        // Deleting the namespace also destroys its TUN device and every
        // iptables rule inside it — nothing else to clean up per cell.
        self.teardown_cell_net(&cell.id).await;
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
