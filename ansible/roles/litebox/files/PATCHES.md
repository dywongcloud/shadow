# litebox networking.patch

Applied by `ansible/roles/litebox/tasks/main.yml` via `git apply` right after
cloning litebox at the pinned commit (`litebox_commit` in
`ansible/roles/litebox/defaults/main.yml`), before `cargo build`. Re-diff
against each future commit bump before updating the pin — same discipline as
`vendor/iroh/CHANGES.md`.

## Base

`microsoft/litebox` at `e7984422ce1aab181305ac7b9085c3e84e7bb27c` (the pin at
the time this patch was written, 2026-08-08).

## What it does, and why

Two independent problems, found live on fc-frankfurt and confirmed by
research against litebox's own source
(`crates/hive-backend/src/litebox.rs`'s module doc, "Networking" section, has
the full narrative):

1. **Wildcard bind never worked.** `litebox/src/net/mod.rs`'s `bind()` (TCP
   and UDP) and the implicit-bind-on-`listen()` path all unconditionally
   built `smoltcp::wire::IpListenEndpoint { addr: Some(addr), .. }`, even
   when the guest asked for the wildcard address (`0.0.0.0`/omitted). smoltcp
   itself already has correct wildcard-listen support
   (`IpListenEndpoint.addr: Option<Address>`, `None` = "any address",
   confirmed in smoltcp 0.12.0 — litebox's own exact pinned version) — this
   was purely litebox's integration code never using the `None` sentinel it
   was already given the means to use. The patch changes all three sites to
   map an unspecified address to `None` instead of `Some(0.0.0.0)`. This is
   the root-cause fix for what this repo's docs call "Problem 2" — and it
   fixes EVERY guest language (the interface has exactly one real address,
   the cell's assigned IP, so `None` correctly matches every real inbound
   packet), not just Node/Bun.
2. **The guest's own IP/gateway were hardcoded at compile time**, with no
   override of any kind — `litebox/src/net/mod.rs`:
   `const INTERFACE_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);` /
   `const GATEWAY_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);`, both
   already marked `// TODO: Make this configurable` by litebox's own authors.
   Every concurrent litebox process therefore believed it was the identical
   address — unusable for a platform running many concurrent sandboxed
   instances that each need their own reachable identity. The patch adds
   `Network::new_with_addrs`/`LinuxShimBuilder::build_with_net_config`
   (additive — the existing zero-arg `new`/`build` are now thin wrappers
   calling these with `None, None`, so every other caller, including
   litebox's own test suite, needs no changes) and wires
   `litebox_runner_linux_userland`'s `run()` to read
   `LITEBOX_GUEST_IP`/`LITEBOX_GATEWAY_IP` from the environment. Unset =
   byte-identical to upstream behavior.

## Why not wait for upstream, and why not switch to the `ulitebox` branch

A parallel, unreleased litebox rewrite (branch `ulitebox`, an actively
churning personal branch of a Microsoft engineer, not on `main`, no
merge-to-main PR as of this patch) replaces the whole smoltcp/TUN stack with
a broker process issuing real host socket syscalls — a more thorough fix for
loopback, but its own `authorize_socket_bind` policy hard-DENIES wildcard
binds by design (confirmed via its own unit test), so it would not remove
the need for problem 1's fix even if adopted. It is also unstable and
undocumented. Not worth the risk for production; worth revisiting only once
it stabilizes and merges to `main`.

## Verified

`cargo check -p litebox` (macOS/aarch64, so most of the crate's
`target_os = "linux"`-gated code doesn't compile there either way) shows the
same 11 pre-existing, patch-unrelated errors (an x86_64-specific exception
table macro) before and after this patch — confirmed via `git stash`. The
three files this patch touches compile clean in isolation. Full functional
verification (the wildcard-bind fix and the multi-instance addressing both
actually working) happens via `hive-cloud --litebox-probe`'s network check on
a real x86_64 Linux host, not locally.

# litebox fork.patch

Applied right after `networking.patch` (same task sequence, same idempotent
apply-every-run discipline — the clone step always resets tracked files to
pristine first). Composes cleanly with `networking.patch` — disjoint files,
verified by applying both in sequence against a fresh pinned clone.

## Base

Same pin as `networking.patch`: `microsoft/litebox` at
`e7984422ce1aab181305ac7b9085c3e84e7bb27c`.

## What it does, and why

Adds a **rough v0** of real process `fork()` support, for the Sandboxes
interactive-terminal feature (`crates/hive-cloud/src/sandboxes_platform.rs`'s
`open_shell` / the guest-side `ExecPty` protocol in
`crates/hive-cell-agent`) — a shell needs to `fork()`+`exec()` every command
a user types (`ls`, `cat`, `vim`, ...), and litebox had **no process concept
at all** before this patch, confirmed by direct source reading:

- `litebox_shim_linux/src/syscalls/process.rs`'s `do_clone` REQUIRED
  `CLONE_VM | CLONE_THREAD | CLONE_SIGHAND | CLONE_FILES` — thread-only.
  Guest "threads" are real host pthreads sharing ONE
  `litebox_runner_linux_userland` process's real address space (guest
  `mmap`/`brk` are real host `mmap` calls — the syscall-rewriting model means
  the guest's memory literally IS host process memory).
- `sys_execve` tears down and replaces the CURRENT guest state in-place — an
  in-process ELF loader, never spawns anything new.
- **No `wait4`/`waitid` implementation existed anywhere in the crate** —
  confirmed by exhaustive grep. A forked shell reaping its child would
  currently get `ENOSYS`. This patch does **not** fix that gap — see
  "What this does NOT cover" below.

### The design: a real host `fork(2)`, not an emulated one

`litebox_shim_linux` is `#![no_std]` with no real OS access at all — a raw
`fork()` syscall is structurally impossible to issue from `do_clone` itself.
The only crate with real host access is `litebox_platform_linux_userland`,
already reached from `do_clone` via the existing
`self.global.platform.spawn_thread(...)` call (the `ThreadProvider` trait
hook every guest-thread-clone already goes through). This patch adds a
sibling trait method, `ThreadProvider::fork_process`, with a
default-unsupported implementation (every platform except a real
fork()-capable host OS process — Windows userland, bare-metal kernel, SNP,
LVBS, OP-TEE — inherits this unchanged, matching every other
optional-capability default already in this trait). `LinuxUserland` is the
one implementer: it calls the real `libc::fork()` and returns a
`ForkOutcome::Parent(child_pid)` / `ForkOutcome::Child` enum mirroring
POSIX `fork()`'s own return-value contract.

`do_clone` detects the real-`fork()` shape (glibc lowers `fork()` to
`clone(SIGCHLD, NULL, ...)` on x86_64 — i.e. a `Clone` request with **no**
flags at all, not even `CLONE_VM`) as an early branch before the existing
`required_clone_flags` rejection, calls `fork_process`, and returns 0 in the
child / the child's real host pid in the parent — exactly POSIX `fork()`
semantics. `vfork()` (`CLONE_VM | CLONE_VFORK`) is deliberately **not**
handled — it shares the parent's address space until `exec()`, a genuinely
different and harder problem than a real copy, and falls through to the
existing unsupported-flags rejection unchanged.

### What this v0 does NOT cover (load-bearing, not hidden)

- **Single-guest-thread-at-fork-time ONLY.** A real host `fork()` continues
  only the calling thread into the child; every OTHER guest pthread this
  process's `spawn_thread` may have started simply vanishes in the child
  while litebox's own `Process`/`ThreadState` bookkeeping still believes
  they exist. `do_clone` checks `Process::nr_threads() == 1` (a pre-existing
  public accessor, an atomic already tracked for `wait_for_exit`) before
  calling `fork_process` and returns `ENOSYS` otherwise — a real, load-bearing
  guard, not a TODO. A shell immediately after launch (before it starts any
  of its own background threads) satisfies this; a shell mid-job-control
  with backgrounded jobs holding extra threads would not.
- **No fork-safety audit against locks held by a DIFFERENT host thread at
  the instant of `fork()`.** Only the calling thread survives into the
  child; any `Mutex`/`RwLock` (e.g. `Process::inner`) held by another thread
  at that exact moment is permanently poisoned/deadlocked in the child. This
  patch's own doc comments on `ThreadProvider::fork_process` name this
  explicitly as unaudited — a real audit of `Process<Platform>`'s and
  `ThreadState<Platform>`'s lock usage against realistic concurrent syscall
  timing is genuinely separate, harder work this patch does not attempt.
- **No `wait4`/`waitid` implementation.** litebox has zero wait-family
  syscall support today; a forked child's exit status cannot currently be
  reaped by the guest shell at all. A real shell needs this to report exit
  codes and avoid zombie accumulation — without it, this fork() is real but
  incomplete for actual interactive use. Tracked as explicitly open, not
  silently assumed away.
- **`vfork()` unsupported** (see above — falls through to the existing
  rejection, unchanged behavior).
- Every platform except Linux userland (Windows userland, bare-metal kernel,
  SNP, LVBS, OP-TEE) inherits the default `Unsupported` — no fork() anywhere
  but the one platform this patch actually targets.

## Why a real host fork() instead of reimplementing it in the emulator

The alternative — building fork semantics entirely inside litebox's own
guest-memory emulation layer — would mean re-inventing copy-on-write address
space duplication from scratch. Since litebox's guest memory already IS real
host process memory (the syscall-rewriting model, not a software MMU), a
real host `fork(2)` gets that exact semantic for free from the host kernel.
This is the same reasoning `networking.patch` already documents for why a
small forked patch beats fighting litebox's architecture from outside it.

## Verified

`cargo check --target x86_64-unknown-linux-gnu` (real Linux target, not the
macOS-limited surface `networking.patch`'s own verification was confined to)
for `litebox`, `litebox_shim_linux`, `litebox_platform_linux_userland`,
`litebox_platform_linux_kernel`, `litebox_platform_multiplex`, and the real
`litebox_runner_linux_userland` binary crate — all clean, both standalone
and with `networking.patch` applied first (the actual deployment order).
`git apply --check` confirmed against a completely fresh clone at the pinned
commit, standalone and stacked on `networking.patch`. NOT verified: actual
runtime behavior (no real Linux box with a litebox-capable kernel was
available in this session) — a fork() that type-checks and a fork() that
correctly hands off guest execution to a real, distinguishable child process
are different claims, and only the first one is proven here. Needs a real
`--litebox-probe`-style functional test (spawn a guest shell, fork it, exec
`/bin/true` in the child, observe the parent still running) on actual
target hardware before this is anything more than a structurally-sound
starting point.
