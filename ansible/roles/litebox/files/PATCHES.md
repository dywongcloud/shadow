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

## 2026-09-04: upstream history wiped — the above provenance evidence is now historical, not re-derivable

`AnEntrypoint/litebox`'s ENTIRE git history was replaced with a single
squashed "Initial commit" (repo `created_at` == commit date ==
2026-09-03T14:11:38Z, confirmed via the GitHub API; the previously-pinned
`19532929bbe59769ce9653fdde3c69852d85b9b3` returns 422 "No commit found" —
it no longer exists anywhere on GitHub). The "500+ commits, real dated
history" and "85 real, individually dated commits for fork_process" claims
above describe history that genuinely existed and was genuinely checked at
the time — they are not fabricated — but that history is now GONE and
cannot be re-checked by a future session; do not cite those specific commit
counts as still-verifiable.

Re-verified legitimacy a different way before re-pinning to the new tip
(`c325d5d9d446948b2b9030a3589bbee482b58c88`), not taken on faith: (1) the
repo's `created_at` timestamp exactly matches the sole commit's timestamp,
consistent with a genuine delete-and-recreate rather than a hidden partial
history; (2) the commit author `lanmower` is a real, long-standing GitHub
account (created 2011, 34 followers, 201 public repos) and a genuine
current member of the `AnEntrypoint` org (`GET orgs/AnEntrypoint/members`),
whose own bio references this same platform's `gm`/`plugkit` tooling; (3)
the new checkout's README, file layout, and full crate structure
(`litebox_shim_linux`, `litebox_platform_linux_userland`,
`litebox_runner_linux_userland`, etc.) are unambiguously the same real
LiteBox project, not a name-squat with unrelated content; (4) this
directory's own `networking.patch` applies CLEANLY (`git apply --check`,
exit 0, zero changes needed) against the new tree — real structural
corroboration that the surrounding code this patch targets is materially
unchanged, not a rewrite that happens to share a name. What this does NOT
establish: whether the specific fork()/wait4()/pty fixes documented above
are still present in the new squashed commit's code — re-run this
document's own "spot-read `litebox_shim_linux/src/syscalls/process.rs`,
confirm `required_clone_flags` no longer exists" check against the new pin
before trusting those specific claims again, and re-run
`hive-cloud --litebox-probe` on a drained canary before extending
`HIVE_LITEBOX_VERIFIED=1` to any additional node on this new pin.

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

## 2026-09-09: hunks 4-8 -- the guest can write into its own application tree again

**Symptom (live, fc-sanjose, the day it flipped mock -> Litebox):** every
deployment that creates a file at runtime died at launch with
`EACCES: permission denied, mkdir '/workspace/server/data'` (a JSON storage
adapter's `fs.promises.mkdir`); a bare `open(O_CREAT)` in the same directory
did not even get an errno -- the runner panicked (`layered.rs:683`
`Result::unwrap()` on `NoWritePerms`, exit 101).

**Root cause, measured with a standalone runner + hand-built ustar (see the
`lb-eacces-repro*.py` / `lb-node-witness.py` scripts this session left on the
host), never inferred from mode bits:** the guest runs as ONE fixed non-root
uid (1000, `DEFAULT_GUEST_UID`); the `--initial-files` tar is a read-only
lower layer whose directories are all synthesized 0777 root-owned (explicit
tar directory entries are ignored, so the sealed artifact's 0555 modes are
irrelevant); creating anything inside such a directory first migrates its
ancestors into the writable in-memory upper layer
(`layered.rs::mkdir_migrating_ancestor_dirs`), and the first migrated
ancestor is created directly under the upper root `/` -- which
`in_mem::RootDir::new` creates root-owned 0755. uid 1000 cannot write there,
so the migration fails and EVERY write into `/workspace/...` fails, while
`/tmp` (created world-writable under `with_root_privileges` at setup) works.
Seeding the upper layer via `--resume-from` is not a workaround: its importer
neither honours directory entries nor creates parents (`PathError
(MissingComponent)`). The pre-fork microsoft runner (`e7984422`, still on
fr/phx/sj3/4/5) allows mkdir and create in these directories (it panics on
truncate/append of a shipped file and returns ENOSYS for `rename`, so it is
not a clean baseline either) -- the mkdir/create regression is fork-introduced,
the same DAC-strictness family hunks 1-3 already patch for uid 0.

- **Hunk 4 (`litebox_runner_linux_userland/src/lib.rs`):** after the `/tmp`
  bootstrap, `chmod("/", 0777)` under `with_root_privileges`. The guest is the
  only principal in its private in-memory tree (the sandbox boundary is
  seccomp + the rewriter, never in-mem DAC), so a writable root protects
  nothing and denies the guest its own cwd. Restores exactly the microsoft
  runner's observable behaviour (`mkdir /scratch` and `mkdir
  /workspace/newdir` succeed, `/tmp` unchanged).
- **Hunk 5 (`litebox/src/fs/layered.rs`):** the `O_CREAT` path's ancestor
  migration maps `MkdirError` into `OpenError` instead of `.unwrap()`ing --
  a guest sees EACCES/EROFS/EIO, never a dead runner. The rename/link/symlink
  migrations already propagate; `mkdir` uses `?`.

An adversarial review (four refute-only agents, sixteen independent
verifiers, 2026-09-09) did not refute the root cause or hunks 4-5 but
witnessed two more fork defects that hunk 4 makes REACHABLE for every
application, plus panics hunk 5 did not cover. All fixed in `layered.rs`,
all re-witnessed with the reviewers' own scripts on the rebuilt runner
(`ba293cc870250604…`):

- **Hunk 6 (`rename`):** renaming over a file that exists in the tar layer
  inserted a *tombstone* at the target after the upper rename succeeded --
  and `open`/`file_status` consult the tombstone BEFORE the upper layer, so
  the freshly renamed file answered ENOENT to every non-`O_CREAT` open while
  `readdir` still listed it (the write-tmp-then-`rename()` atomic-save idiom
  every JSON store uses "succeeded" and then lost its file; the microsoft
  runner returns ENOSYS for rename instead). Fix: drop the stale cached entry
  (what the no-shadow branch already did) -- the upper file shadows the
  lower one exactly as the design doc describes. Witness: `rename=ok |
  read={"v":"NEW"} | stat=11 | access=ok` (was `read=ENOENT | stat=ENOENT`).
- **Hunk 7 (`migrate_file_up`):** the copy-up created the upper file with
  the LOWER's mode. A sealed hive artifact ships every file with the write
  bits stripped (0444, a host-integrity measure, not the app's intent), so
  the very next write-open of the guest's own fresh copy failed
  `AccessNotAllowed` and the unwrap killed the runner (`layered.rs:377`,
  exit 101) for any app rewriting a file that shipped in its repo. The copy
  is owned by the guest uid, so it now carries `WUSR`. Witness: overwrite /
  append / `r+` / chmod+write of a 0444 shipped file and of
  `/workspace/.hive-runtime-artifact-v1.json` all `OK` (were all exit 101).
- **Hunk 8 (three sites):** the ancestor migration inside `migrate_file_up`
  (`unimplemented!` at `:259`), the copy-up create (`.unwrap()` at `:269`,
  now `ReadOnlyFileSystem -> UpperCannotHoldPath` so an outer layer can take
  it) and the descriptor re-open in the swap loop (`.unwrap()` at `:377`)
  all report `MigrationError` instead of panicking. `Io` rather than a new
  permission variant because every caller maps `Io` and treats
  `NoReadPerms` as `unimplemented!()` -- closing those caller arms is a
  follow-up (PRD `litebox-migration-noreadperms-callers`).

Residuals the review recorded and this change deliberately leaves alone
(PRD rows, all pre-existing on every runner): a root-owned 0600 tar file is
shadowed by `O_APPEND|O_CREAT` instead of EACCES; `--export-writable-layer`
/`--resume-from` round trip is broken in hive's invocation shape (no hive
caller); a 0777 root has no sticky bit, so the guest may `rmdir` the
platform-created `/tmp` (it could already unlink/tombstone any platform file
on the shipped runner -- the sandbox boundary is seccomp + the rewriter,
never in-mem DAC, and no host-side consumer trusts guest-visible files after
launch); coreutils `touch` fails `utimensat(NULL)` with EFAULT.

**Delivery facts the review corrected:** the live fleet was 7 nodes at
review time and fc-sanjose the ONLY live AnEntrypoint-pin Litebox node
(tokyo closes SSH, seoul presents a changed host key -- not accepted,
sj6/va4/va5/cvm-1/2 down); fr/phx/sj3/4/5 run the microsoft pin and are not
affected by the mkdir regression. Two hazards fixed alongside: the role's
`git` task could not fetch the dangling pin at all (`ansible.builtin.git`
never fetches a bare sha without `refspec`) and an un-`--limit`ed `--tags
litebox` run would have stripped fc-phoenix's `litebox-verified.conf`
(inventory line lacked the flag while the node runs verified) and restarted
it into MockBackend. The probe (`hive-cloud --litebox-probe`, check (c))
now writes into a tar-layer directory, overwrites a shipped 0444 file and
does the atomic rename, so none of these classes can PASS a probe again.

Witnessed on fc-sanjose with the rebuilt runner (`49b592c9e0f413d9…`, old
pin tree + patch): the exact SurveyBot pattern as a Node one-liner
(`fs.promises.mkdir('/workspace/server/data',{recursive:true})` + write +
read-back + `readdirSync`) -> `FAIL EACCES` on the shipped runner, `WROTE
{"ok":1} DIR ['data','server.js']` exit 0 on the fixed one.

**Pin state at the time of this change, recorded so the next session does
not re-derive it:** `AnEntrypoint/litebox` main was rewritten AGAIN between
2026-09-03 and 2026-09-07 -- it now carries 1331 commits (the pre-wipe history
restored, HEAD `f62ca45c` 2026-09-07 by `lanmower <admin@coas.co.za>`, GitHub
login on that commit `imraanlockhat`, org members `jb0gie`/`lanmower`). The
pinned `c325d5d9` is a DANGLING commit: not on any ref, so a plain clone
cannot check it out, but `git fetch origin <full-sha>` still retrieves it (the
role's `git` module fetches by sha, so the role still builds) -- GitHub may
garbage-collect it at any time. Hunks 1-5 apply cleanly to `c325d5d9`;
against current main only the runner `lib.rs` hunk 1 no longer applies.
Re-pinning to a reachable commit needs the six-step legitimacy check
(`litebox-upstream-history-wipe-20260904` memory) and a rebase of that one
hunk -- deliberately not done inside a production incident.

**Landed on fc-sanjose 2026-09-10 (operator-approved):** `/usr/local/bin/
litebox-runner` = `ba293cc870250604…` (sj's own current source tree at the
old pin `19532929` + hunks 1-8 -- the pin the role now builds, `c325d5d9`,
takes the identical 17-hunk `networking.patch`); previous runner kept at
`/usr/local/bin/litebox-runner.old-b9cf16fbf666605f`; hardened probe PASS
against the installed binary; hive-node NOT restarted (running cells keep
their runner, new cold starts pick up the fix). End-to-end witness:
`survey-botdemo` (the reporting project, every build of which had died at
launch) deployed through the real `/v1/git/deploy` path as `dpl-ff8f276ca4`
-> `ready`, deployment `dpl-d171d4ffa4`, `https://survey-botdemo.shadw.app/`
answering 200 `SurveyBot Dashboard` through the public round-robin host.
Review residue kept on fc-sanjose for the next session: the exact built tree
`/root/litebox-fix-src-oldpin` (the only copy of the old-pin sources -- that
commit no longer exists upstream), the pin clone `/root/litebox-fix-src`,
the probe build `/root/hive-cloud-probe-bin`, and the witness scripts
`/root/lb-*.py`, `/root/adv-*.py`.

## 2026-09-22: Fork patch (`fork-inplace.patch`) -- a second external command no longer wedges the shell

Applied AFTER `networking.patch` by the role (`git apply ../litebox-fork-inplace.patch`), touching only
`litebox_shim_linux/src/{lib.rs, syscalls/mm.rs, syscalls/process.rs}` (180 lines). Root-caused by a
subagent with gdb on va (see the `litebox-fork-child-corruption` memory): `do_clone` gives a plain
`fork()` an eager copy of the guest's writable memory at RELOCATED host addresses and runs the child on a
second host thread. Only registers, the TCB self-pointer, a 4 KB stack window and a few writable ELF data
words are patched (`fixup_stale_stack_pointers`/`fixup_stale_elf_data_pointers`); everything else in the copy
still points into the PARENT (RELRO/`.got`/`.data.rel.ro`/stdio vtables -- Rocky's bash is full-RELRO -- and the
whole heap). The child runs a mix of its own and the parent's libc and rewrites the parent's `_rtld_global`
(stack lists), malloc and stdio state: `malloc(): unaligned tcache chunk`, `glibc detected an invalid stdio
handle` (`_IO_vtable_check` failing on a translated vtable pointer), and the second child spinning in
`__libc_fork`'s inlined `reclaim_stacks` walk over a stack list the first child corrupted.

Three hunks:
1. **process.rs, in-place fork.** A plain `fork()` of a SINGLE-THREADED caller snapshots its writable non-shared
   regions (stack only from `rsp-512` up; refused above 256 MiB), runs the child through the existing
   `CLONE_VFORK` path (shared page manager, caller suspended until the child execs or exits, no relocation), and
   restores the snapshot afterwards; the child does not write a clear-tid word into shared memory. Multi-threaded
   callers (Node, Python with threads) and over-cap callers keep the old relocating copy.
2. **mm.rs, `sys_brk`.** A `brk` it cannot satisfy answers the unchanged break, not `-ENOMEM` (brk has no errno; glibc
   read the errno as a huge break and `sysmalloc` wrote into unmapped pages).
3. **lib.rs, trap fallback.** The syscall rewriter leaves `icebp; hlt` (`f1 f4`) where it cannot hook a `syscall` (the
   trampoline page is taken when two processes load at once: `applied trap fallback count=533`); the fallback now
   serves the syscall through the shim exactly as the hooked path does and resumes, instead of killing `head` in
   25-30% of `ls | head` runs.

**Known limits (accepted, measured):** the parent is suspended until the child execs or exits, so a NON-exec child
that needs the parent to progress deadlocks -- `v=$(for i in $(seq 1 30000); do echo line$i; done)` hangs once the
output passes the 64 KiB pipe; `(sleep 1; echo x) & echo y` prints `x` first; cost is O(writable RSS) per fork; if a
child unmaps a parent region before exec the restore write fails (logged) and the parent may fault later; aarch64
branches are unbuilt. Upstream tip (`3418c51f`, 2026-09-19) moved native Linux to a real host `fork()` but the Linux
platform never implemented `wait_for_cross_process_exit`/`spawn_cross_process_exit_notifier` (every forking command
exits 101 `unreachable!`); a 45-line waitpid spike got as far as `threads should not terminate unexpectedly`. Re-pin
only after that lands, then drop this patch. Incidental, not fork-related, still open: the `time` builtin prints
garbled user/sys times (rusage/`wait4`), and every litebox runner burns 25-100% of a core idle (net poll thread).

## 2026-10-09: TCP keep-alive patch (`tcp-keepalive.patch`) -- an in-flight connect must not die on TCP_KEEPIDLE

Applied THIRD, AFTER `networking.patch` and `fork-inplace.patch`, by the role
(`git apply ../litebox-tcp-keepalive.patch`). Touches only
`litebox_shim_linux/src/syscalls/net.rs` (61-line diff, 2 hunks).

**The bug.** `setsockopt(TCP_KEEPIDLE)` / `TCP_KEEPCNT` (and `TCP_KEEPINTVL`) were answered
`EOPNOTSUPP`. libuv's `uv__tcp_keepalive` -- which backs Node's `socket.setKeepAlive` and therefore
undici's global `fetch` -- issues `SO_KEEPALIVE` and then these options immediately after
`connect()`, while the socket is still in SYN_SENT. A kernel stack accepts all of them and lets the
connect finish; returning `EOPNOTSUPP` here made libuv fail the *pending connect* with ENOTSUP, so
every Node outbound `fetch()` in a guest failed before the TCP handshake could complete.

**The change.** All three are accepted and ignored (a `debug!` line, then `Ok(())`), and the old
`TcpOption::KEEPINTVL` arm that forwarded the value into smoltcp's `set_keep_alive` is removed.
Ignoring rather than applying is deliberate, not laziness: `uv_tcp_keepalive` is
`uv_tcp_keepalive_ex(handle, on, idle, 1, 10)` -- a HARDCODED 1-second interval -- so an applied
value produces real probes, one per second per idle socket across undici's connection pool. The
2-hour default that `SO_KEEPALIVE` already installs is kept. `TCP_INFO` stays `EOPNOTSUPP` (it is
get-only on Linux).

**Why a third patch file instead of folding it into `networking.patch`.** It is a different defect
class (socket-option errno policy, not addressing/guest-IP configuration), it has to land AFTER the
other two so the role's apply order stays meaningful, and a re-diff of one must not invalidate the
others when the pin moves.

**Verified 2026-10-09** (see PRD `litebox-tcp-keepalive-third-patch`): all three patches
`git apply --check` cleanly, in order, against a clean clone of the pin `c325d5d9`; the resulting
tree is byte-identical to the deployed `/root/litebox-fix-src` on fc-sanjose
(`diff -rq --exclude=target --exclude=.git` -> no differences; `net.rs` sha256
`5465f8971e55a15d54c65dea5688650ffdaeb3409c4412259852ace2ce79e36c` on both sides). Deployed runner
sha256 `6ae607e8dd783a6062caf766f09c7ace75ea0bfb7319e03e18d51c3c6318f6c0` (glibc 2.39) on both live
litebox nodes, fc-phoenix first and fc-sanjose last. Live: `TCP_KEEPIDLE`/`TCP_KEEPCNT` now return
OK instead of errno 95, a Node global `fetch` to a single-A-record destination completes (308 round
trip), and `getsockopt(TCP_KEEPINTVL)` after setting 1 returns 7200 -- the 2-hour default survives,
so there is no probe-per-second.

**Note on the pin.** This patch was re-diffed against the pinned `c325d5d9` tree ON TOP OF the other
two patches, from the tree that produced the deployed binary. It contains nothing else: the two
committed patches account for exactly the other eight modified files in that tree
(`litebox/src/fs/{in_mem,layered,resolver}.rs`, `litebox/src/net/mod.rs`,
`litebox_runner_linux_userland/src/lib.rs`, `litebox_shim_linux/src/{lib.rs, syscalls/mm.rs,
syscalls/process.rs}`), so `net.rs` is the only file this patch adds.
