# PVM 6.12→7.1 port: bug report for dywongcloud/pvm-no-fsgsbase-rdtscp

Findings from bringing up `patches/pvm6.12to7.1complete.patch` on a fresh
Rocky 10 host (fc-frankfurt, 162.62.83.144 — a Tencent CVM with no `vmx`/`svm`,
64 vCPU EPYC 9K65, 246 GiB RAM, 1 TiB disk), 2026-07-31. Six real bugs were
found and worked around enough to boot the kernel and run containers; the
seventh is a live, unresolved **host-crashing** bug and is why this node is
fenced off from Firecracker workloads (see `docs/blocked-work.md`) until a
kernel engineer — not an agent hand-patching blind — traces the actual root
cause.

Status of this fleet's own copy: the kernel is BUILT, BOOTED, and serving
production container traffic on this node right now (`kvm_pvm` loaded,
`/dev/kvm` present, real `KVM_CREATE_VM`/`KVM_CREATE_VCPU` ioctls succeed,
podman/crun and gVisor containers both run to completion). What does **not**
work is the one thing PVM exists for: a real guest-mode transition. Booting a
Firecracker microVM resets the whole host, verified reproducible (3 times,
see below) — so this report is genuinely "the platform bring-up succeeded and
the payload crashes it," not a general kernel-doesn't-boot report.

## 1. UAPI header: kernel-internal typedefs in an exported header

`arch/x86/include/uapi/asm/pvm_para.h` declares `struct pvm_vcpu_struct`
fields using kernel-internal typedefs (`u64`/`u32`/`u16`) instead of the
UAPI-safe `__u64`/`__u32`/`__u16`. These aliases don't exist outside the
kernel build, so `make headers` / the `usr/include` export (`hdrtest`) fails
hard: `unknown type name 'u64'` ×9.

**Fix applied:** `s/u64/__u64/`, `s/u32/__u32/`, `s/u16/__u16/` across the
struct.

## 2. UAPI header: missing `#include <linux/types.h>`

Same file: after fix #1, `hdrtest` still failed — `unknown type name
'__u64'` — because the header only pulled in `<linux/const.h>`, not
`<linux/types.h>` (which is where `__u64`/`__u32`/`__u16` are actually
defined). The sibling `kvm_para.h` includes it; this header didn't.

**Fix applied:** added the missing `#include <linux/types.h>`.

## 3. UAPI header: C++-style comments fail strict ISO C90 hdrtest

Same file, again: three lines of `//`-style comments on deprecated MSR
defines. `hdrtest` compiles UAPI headers under a strict ISO C90-ish flag set
that rejects `//` comments outright.

**Fix applied:** converted to `/* */` block comments.

**All three of the above are in one file and none would have been caught by
a normal `make` — only `make headers && ` a real hdrtest pass finds them.**
That suggests the fork's patch series was never actually run through kernel
CI's standard UAPI-export gate before merge. Recommend bundling all three as
one PR against the header.

## 4. musl toolchain: UAPI headers missing for C shims (not PVM-specific)

Building `firecracker-next`'s musl-target release needs `linux/types.h`
etc. under the musl sysroot for at least two dependencies' C shims:
`libseccomp` (built from source — Rocky 10 only ships it as a shared lib,
no static `.a`) and `userfaultfd-sys`'s `consts.c`. `musl-gcc` has no
bundled Linux UAPI header tree.

**Fix applied:** a private copy of the host's `/usr/include/{linux,asm,
asm-generic}` passed via `-idirafter` (NOT plain `-I`, which shadows musl's
own libc headers with glibc's and breaks `va_list`) — for `libseccomp` via
its configure line, and for every C dependency in the firecracker-next
workspace via `CFLAGS_x86_64_unknown_linux_musl` so `cc-rs` picks it up
uniformly. This is a build-environment note, not a kernel patch, but it's
included here because it blocked `firecracker-next` builds identically to
the header bugs above and is worth documenting alongside them if this fork
is ever the basis of a packaged prerequisites role.

## 5. Host switcher assembly: CET-IBT violations

`arch/x86/entry/entry_64_switcher.S` (the patch's hand-written host-switcher
assembly) trips `objtool`'s Indirect Branch Tracking check: `unsupported
instruction in callable function` at `common_interrupt_return+0xee`, plus
three `relocation to !ENDBR` faults targeting `entry_SYSCALL_64_switcher+
0xcf` (×2) and `switcher_enter_guest+0x0`. Indirect/data jumps into the
switcher land on offsets that aren't `ENDBR64`-landed.

**Worked around, not fixed:** `CONFIG_X86_KERNEL_IBT=n` for this build — a
real hardening feature turned off, not a cosmetic warning suppression.

**Proper fix needs either:** (a) `ENDBR64` landing pads at the actual
indirect-jump targets inside the switcher (requires tracing every
`SWITCH_TO_*` control-flow edge the patch adds), or (b) an `objtool --ibt`
exception/annotation if the switcher's control flow is provably safe by
construction. Until fixed, IBT is off system-wide on any node running this
kernel — a real, if narrow, hardening regression.

## 6. `objtool`: unsupported instruction in `common_interrupt_return`

After fix #5, one `objtool` error remained: `common_interrupt_return+0xee:
unsupported instruction in callable function` — an instruction inside the
shared IRET/syscall-return path (`entry_64.S`) that the patch's
`CONFIG_PVM_GUEST` `ALTERNATIVE` branch (`jmp
pvm_restore_regs_and_return_to_usermode`, defined in the separate
`entry_64_pvm.S`) reaches into.

Could not pin down the exact instruction in this session: `vmlinux.o` is
deleted by Make's normal recipe-failure cleanup on a failed build, and
`objtool` runs post-link with `--werror`, so nothing survives a failed run
to disassemble offline.

**Worked around, not fixed:** `CONFIG_OBJTOOL_WERROR=n` (kept
`CONFIG_HAVE_NOINSTR_VALIDATION` off too), so this is a non-fatal warning
instead of a build stopper. The generated machine code at that site is
**unchanged** either way — only `objtool`'s CFI/stack-unwind metadata
coverage for that instruction is incomplete, which affects reliable stack
unwinding (perf, oops backtraces, livepatch) through that path, not runtime
correctness by itself.

**Proper fix:** re-run with `objtool`'s failure captured non-fatally (patch
`scripts/Makefile.vmlinux_o`'s `cmd_ld_vmlinux.o` to `|| true` around the
`objtool` call for one diagnostic build, or `make -k`) so the object file
survives, disassemble `entry_64.o` at offset `0xee` within
`common_interrupt_return`, identify the actual instruction, and add the
correct `STACK_FRAME_NON_STANDARD`/`UNWIND_HINT` annotation or fix the
sequence.

## 7. vDSO RDTSCP fast-path dropped during the 6.12→7.1 port (perf-only, not a correctness bug)

`0001-x86-pvm-vdso-getcpu-lsl-fallback.patch`'s vDSO RDTSCP fast path (a PVM
guest without RDPID falls back to LSL instead of RDTSCP when the host hides
RDPID) didn't survive the 6.12→7.1 port: 7.1 removed the
`alternative_io_2` macro the patch used. A from-scratch inline
`ALTERNATIVE_2` asm block was attempted and reverted rather than shipped,
because it could not be verified against a real PVM-hidden-RDPID host in
this session and a wrong output-constraint/clobber list in a syscall-hot
`vdso_read_cpunode` path is a correctness risk, not a cosmetic one.

**Current state:** reverted to the safe vanilla `alternative_io` LSL/RDPID
form — correct, just missing the RDTSCP fast path (a `getcpu()`
micro-optimization only). `CONFIG_KVM_PVM_GUEST`/
`X86_FEATURE_PVM_RDTSCP` flag-setting in `cpu/common.c` is harmless dead
code, left in place.

**To restore properly:** a verified x86_64 `ALTERNATIVE_2`/`alternative_io`
macro (or equivalent hand-written `asm_inline` + `ALTERNATIVE_2` with
correct constraints) needs to be tested on real hardware where RDPID is
genuinely absent but RDTSCP+PVM are present — not blind on a build host
with neither feature to exercise.

## 8. CRITICAL, UNRESOLVED: a real guest-mode transition crashes the whole host

**This is the blocker.** The kernel is otherwise functionally KVM-capable —
`KVM_CREATE_VM`/`KVM_CREATE_VCPU` ioctls succeed, real containers run — but
booting an actual Firecracker microVM (`firecracker-next`, a real
`vmlinux`+rootfs, 2 vCPU / 512 MiB config) hard-resets the entire host.
**Reproduced three times** on fc-frankfurt on 2026-07-31 (kdump captured all
three; see `/var/crash/` on that host — each `vmcore` is ~1.4 GiB, preserved,
do not delete):

| crash | timestamp (host-local) |
|---|---|
| 1 | 2026-07-31 04:29:54 |
| 2 | 2026-07-31 06:45:05 |
| 3 | 2026-07-31 06:46:50 |

Crash 1's dmesg was captured and analyzed in depth (`vmcore-dmesg.txt`):

```
Oops: int3: 0000 [#1] SMP NOPTI
RIP: switcher_return_from_guest+0x18/0x74
```

`switcher_return_from_guest` is a `SYM_INNER_LABEL` inside
`switcher_enter_guest` (`arch/x86/entry/entry_64_switcher.S:80`), reached via
a far `jmp` from the PVM vmexit/event path at line 329. `CPU 60 comm=fc_vcpu`
— a real Firecracker vCPU thread, mid-vmexit. `kdump=loaded not-tainted`,
and the cmdline carries `panic=5` — a genuine panic would have logged first,
so this is a real fault (not a deliberately-triggered panic path).

The faulting instruction sits inside `MITIGATION_ENTER`'s
`FILL_RETURN_BUFFER` (`arch/x86/include/asm/nospec-branch.h`,
`__FILL_RETURN_SLOT`: `call 772f; int3; 772:`). That `int3` is
**self-correcting by design** — the `call` always falls through past it; the
byte exists only as a landing pad for CPU speculative RSB reads and is never
meant to execute as a real trap under normal control flow. RIP landing
squarely on it as a genuine fault means `%rsp`/control-flow was already
wrong on entry to `switcher_return_from_guest` — i.e. the patch's
stack-switching discipline at the vmexit → `switcher_return_from_guest`
handoff is broken, most likely a `%rsp`/stack-alignment bug in how the far
`jmp` at line 329 (or whatever event/exception path lands there) hands off
to this inner label, corrupting the assumptions `FILL_RETURN_BUFFER`'s
call/int3/add-rsp sequence depends on.

Register dump: `RSP=ffffcc3a8db63f48`, `RBP=ffffffff97402069` — note `RBP`
looks like a **kernel text address**, not a real frame pointer, which may
itself be the smoking gun for stack-frame corruption at the transition.

**This is deeply security/correctness-critical hand-written entry assembly**
(host↔guest transition, speculative-execution mitigations) and should not be
hand-patched further by an agent guessing blind. It needs a kernel engineer
tracing every `SYM_INNER_LABEL` entry path into
`switcher_enter_guest`/`switcher_return_from_guest` against what `%rsp`/
`%cr3` state each caller actually guarantees, cross-referenced against the
vmcore's full register/stack dump. All three vmcores are preserved on
fc-frankfurt at `/var/crash/127.0.0.1-2026-07-31-{04:29:54,06:45:05,
06:46:50}/` for exactly this.

## Practical impact

`firecracker-next` — and by extension any Firecracker cell workload on this
platform — **cannot** be used on this PVM 7.1 build until #8 is fixed. The
platform-level bring-up (kernel boots, KVM API present, containers run) is
real, but the actual guest-execution path is not production-safe. This
fleet has structurally disabled it on the affected node (`HIVE_FORCE_MOCK=1`
via a systemd drop-in, forcing the platform's own cell backend auto-select
to the mock backend instead of real Firecracker) rather than relying on
"nothing happens to be scheduled there yet" — see `docs/blocked-work.md`.
