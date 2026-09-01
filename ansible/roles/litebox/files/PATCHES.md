# litebox fork tracking

`ansible/roles/litebox/defaults/main.yml`'s `litebox_repo_url`/`litebox_commit`
point at `AnEntrypoint/litebox` (currently pinned
`19532929bbe59769ce9653fdde3c69852d85b9b3`), not upstream
`microsoft/litebox`. This is a deliberate, user-approved switch — record here
why, what was checked before switching, and what changed.

## Why

The Sandboxes interactive-terminal feature
(`crates/hive-cloud/src/sandboxes_platform.rs`'s `open_shell`, the guest-side
`ExecPty` protocol in `crates/hive-cell-agent`) needs a real interactive Linux
shell inside the guest — a shell that can `fork()`+`exec()` every command a
user types (`ls`, `cat`, `vim`, `tmux`, ...), report exit codes via
`wait4()`/`waitpid()`, and drive a real pty (`setsid()`/`TIOCSCTTY`, raw mode,
job control). Upstream `microsoft/litebox` at the previously-pinned commit
(`e7984422ce1aab181305ac7b9085c3e84e7bb27c`) has **no process concept at
all** — `do_clone` only supports thread creation
(`CLONE_VM | CLONE_THREAD | CLONE_SIGHAND | CLONE_FILES` required), `execve`
tears down and replaces the current guest state in-place rather than
spawning anything new, and there is no `wait4`/`waitid` implementation
anywhere in the crate (confirmed by direct source reading and exhaustive
grep this session, before the switch).

A first-cut, narrowly-scoped patch against upstream (`fork.patch`, real host
`fork(2)`, single-guest-thread-only, no `wait4`) was written and verified
compiling this session, but `AnEntrypoint/litebox` turned out to already
solve the same problem far more completely and was independently verified
(not taken on faith) before switching to it — see "What was checked" below.
The v0 patch is superseded and removed; nothing in this repo references it
anymore.

## What was checked before switching

- **Legitimacy.** `AnEntrypoint/litebox` is a real, MIT-licensed (matching
  upstream's own license), actively developed repository — 500+ commits at
  the time of the pin, spanning real dated history (not a single dump).
  `GET /repos/AnEntrypoint/litebox` reports `fork: false` (it is not a
  GitHub-registered fork of `microsoft/litebox`, i.e. no shared commit
  ancestry via GitHub's fork graph) — its origin/lineage relative to
  upstream was not independently re-derived beyond that; treat it as an
  independent MIT-licensed reimplementation/continuation, not a byte-for-byte
  upstream-plus-patches tree.
- **The feature claims are real, not just README prose.** Searched the
  repo's actual commit history (GitHub commit search) for `fork_process` —
  85 real, individually dated commits (spanning at least 2026-08-11 through
  2026-08-17) implementing and hardening exactly this: `fork()` with correct
  POSIX signal-state isolation between parent and child (a genuine
  correctness bug — sharing `shared_pending`/`handlers` between parent and
  child, matching a real live bug class), `wait4`/`waitid` with a real
  cross-process child registry, `setpgid`/`getpgid` targeting a live
  fork()ed child, `kill()` reaching a live child/process-group. Spot-read
  the actual pinned commit's `litebox_shim_linux/src/syscalls/process.rs`
  directly (not just the README) and confirmed `required_clone_flags` no
  longer exists at all — `do_clone` was substantially rewritten to support
  real `fork()` unconditionally, not gated behind a thread-only flag check
  the way upstream is.
- **Compiles for real.** `cargo check --target x86_64-unknown-linux-gnu` for
  `litebox`, `litebox_shim_linux`, `litebox_platform_linux_userland`, and the
  real `litebox_runner_linux_userland` binary crate (a much larger dependency
  tree than upstream's equivalent check — this build also carries real
  wgpu/winit/wayland desktop-shell support) — all clean, at the exact pinned
  commit, both before and after `networking.patch` (below) is applied.

## What was NOT independently re-verified

Runtime behavior of the fork()/pty/job-control machinery itself (spawn a
guest shell, actually type at it, observe `vim`/`tmux` working) — this
session confirmed the code compiles and the commit history is real and
substantial, not that every claimed fix behaves correctly on real hardware.
That is exactly what `hive-cloud --litebox-probe` (extended to actually spawn
an interactive session, once the CellBackend `exec_pty` implementation for
`LiteboxBackend` lands) needs to prove before `HIVE_LITEBOX_VERIFIED=1` is
set on any node relying on this.

# litebox networking.patch

Applied by `ansible/roles/litebox/tasks/main.yml` via `git apply` right after
cloning litebox at the pinned commit (`litebox_commit` in
`ansible/roles/litebox/defaults/main.yml`), before `cargo build`. Re-diff
against each future commit bump before updating the pin — same discipline as
`vendor/iroh/CHANGES.md`.

## Base

`AnEntrypoint/litebox` at `19532929bbe59769ce9653fdde3c69852d85b9b3` (the pin
at the time this patch was rebased, 2026-08-31 — see "litebox fork tracking"
above for why this base changed from the original `microsoft/litebox` pin
this patch was first written against, 2026-08-08).

## What it does, and why

Two independent problems, originally found live on fc-frankfurt against
upstream `microsoft/litebox` and confirmed by research against litebox's own
source (`crates/hive-backend/src/litebox.rs`'s module doc, "Networking"
section, has the full narrative) — **re-verified against the new
`AnEntrypoint/litebox` base before rebasing this patch**, since the base
switch (above) independently fixed part of problem 1 already:

1. **Wildcard bind.** litebox's `bind()` (TCP and UDP) and the
   implicit-bind-on-`listen()` path all originally built
   `smoltcp::wire::IpListenEndpoint { addr: Some(addr), .. }` unconditionally,
   even when the guest asked for the wildcard address (`0.0.0.0`/omitted).
   smoltcp itself already has correct wildcard-listen support
   (`IpListenEndpoint.addr: Option<Address>`, `None` = "any address",
   confirmed in smoltcp 0.12.0 — the pinned version on both the original and
   the new base) — this was purely integration code never using the `None`
   sentinel it was already given the means to use.
   **`AnEntrypoint/litebox` already independently fixed 2 of the 3 sites**
   (the explicit TCP `bind()` arm and the UDP `bind()` arm both already use
   `addr: None` for an unspecified address, with comments matching this
   patch's own original reasoning nearly verbatim) — confirmed by reading
   the pinned commit's `litebox/src/net/mod.rs` directly before rebasing.
   The one remaining site is the implicit-bind-on-`listen()` path (`listen()`
   called with no prior explicit `bind()`), still using
   `Some(IpAddress::v4(0,0,0,0))` — this patch fixes that one remaining
   site. Fixing it (like the other two) matters for every guest language,
   not just Node/Bun: the interface has exactly one real address, the
   cell's assigned IP, so `None` correctly matches every real inbound
   packet.
2. **The guest's own IP/gateway are hardcoded at compile time**, with no
   override of any kind — `litebox/src/net/mod.rs`:
   `const INTERFACE_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);` /
   `const GATEWAY_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);`, both still
   marked `// TODO: Make this configurable` on the new base too — this part
   of the original patch was NOT independently fixed by the base switch.
   Every concurrent litebox process therefore believes it is the identical
   address — unusable for a platform running many concurrent sandboxed
   instances that each need their own reachable identity. The patch adds
   `Network::new_with_addrs`/`LinuxShimBuilder::build_with_net_config`
   (additive — the existing zero-arg `new`/`build` are now thin wrappers
   calling these with `None, None`, so every other caller, including
   litebox's own test suite, needs no changes) and wires
   `litebox_runner_linux_userland`'s `run()` to read
   `LITEBOX_GUEST_IP`/`LITEBOX_GATEWAY_IP` from the environment. Unset =
   byte-identical to the base's own behavior.

3. **Root DAC override in the guest in-memory filesystem** (added
   2026-09-01, found live on fc-tokyo — a Rocky 10.2 host — the first time
   the `AnEntrypoint/litebox` pin was ever built and probed on a fleet
   node). `hive-cloud --litebox-probe` failed instantly:
   `litebox_runner_linux_userland/src/lib.rs:293:22: called Result::unwrap()
   on an Err value: NoWritePerms`, inside
   `FileSystem::with_root_privileges`. The runner stages the guest program
   by recreating its ancestor directories in the in-mem FS with the HOST's
   mode bits; RHEL-family hosts ship `/usr/bin` as `0555`, and the fork's
   `Permissions::can_write_by` (`litebox/src/fs/in_mem.rs`) honours the mode
   bits even for `UserInfo::ROOT`, so the `CREAT` open of `/usr/bin/echo`
   is refused. Reproduced deterministically: the identical command with the
   program copied under a `0755` directory exits 0 and prints its argument;
   the microsoft `e7984422` runner (which never enforced DAC) passes the
   same probe on fr/phx/sj3. The hunk gives uid 0 Linux `CAP_DAC_OVERRIDE`
   semantics in both permission checkers (`in_mem.rs` `can_read_by`/
   `can_write_by` return true for root, `can_execute_by` requires any x
   bit; the same three in `litebox/src/fs/resolver.rs`). No other user is
   affected. Reported to the fork's maintainer session so the next pin can
   drop this hunk.

## Why not wait for upstream, and why not switch to the `ulitebox` branch

(Reasoning from when this patch targeted upstream `microsoft/litebox`;
unaffected by the base switch to `AnEntrypoint/litebox`, which is a fully
separate concern from the `ulitebox` branch discussed here.) A parallel,
unreleased litebox rewrite (branch `ulitebox` on `microsoft/litebox`, an
actively churning personal branch of a Microsoft engineer, not on `main`, no
merge-to-main PR) replaces the whole smoltcp/TUN stack with a broker process
issuing real host socket syscalls — a more thorough fix for loopback, but its
own `authorize_socket_bind` policy hard-DENIES wildcard binds by design
(confirmed via its own unit test), so it would not remove the need for
problem 1's fix even if adopted. It is also unstable and undocumented.

## Verified

`cargo check --target x86_64-unknown-linux-gnu` for `litebox`,
`litebox_shim_linux`, `litebox_platform_linux_userland`, and the real
`litebox_runner_linux_userland` binary crate — all clean, at the pinned
`AnEntrypoint/litebox` commit, with this patch applied. `git apply --check`
confirmed against a completely fresh clone at the pinned commit.

**Live, 2026-09-01, fc-tokyo (Rocky 10.2, glibc 2.39, no /dev/kvm, idle —
0 containers, 0 cells):** `roles/litebox` cloned the pin, applied this patch
cleanly, built the runner (`cargo build --release`, rustup `stable`
1.98.0), and `hive-cloud --litebox-probe` (the fleet binary the unit runs,
`/root/fc-target/release/hive-cloud`) reported `litebox smoke test: PASS —
both the syscall rewriter AND a full real HTTP round trip through the
per-cell-TUN + patched-litebox + bind-shim networking pipeline succeeded`
in 2 s with zero leftover TUN devices or runner processes. Without hunk 3
the same probe failed instantly (`NoWritePerms`, see above); hunk 3 is the
only difference between the two runs. Runner sha256 at PASS:
`f970bfe70ac86d4e…`. The same day fc-seoul, fc-virginia-4 and
fc-virginia-5 (same OS image, also idle) built a byte-identical runner
(`f970bfe70ac86d4e…` on all three — the build is reproducible across hosts)
from the same pin + patch and each passed the identical probe in 1–2 s.
fc-sanjose-cvm-1 and fc-sanjose-cvm-2 (TencentOS 4, glibc 2.38, rustup
`stable` 1.97.1) built their own identical pair (`cb20b4e9f3cf03f5…` on
both — the digest differs from the Rocky group's only by toolchain/glibc,
never by source) and passed the probe in 3 s each while serving 7 and 29
live containers; `hive-node` stayed active throughout. Full sandbox
exec/PTY behaviour on top of this runner is verified separately per node
before `HIVE_LITEBOX_VERIFIED=1` is set.
