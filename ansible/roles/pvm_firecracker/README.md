# pvm_firecracker

Run Firecracker microVMs **without hardware KVM**, using **PVM**
(Pagetable-based Virtual Machine) on a regular x86_64 cloud VM. This is what
every `[fc_pvm]` inventory host runs (the fleet this repo was built against
has no bare-metal/nested-virt hosts other than the ones in `[fc_kvm]`).

## Provenance

This role was found already built and independently verified end-to-end on
one of this fleet's own nodes (an uncommitted `pvm-firecracker-ansible`
directory under a prior operator's home directory), then reviewed file-by-file
and imported here with explicit sign-off. It is not a random third-party
download -- it is this project's own PVM engineering effort, just not
previously checked into git. See `[[virginia-pvm]]` / `[[fc-virginia-3]]`-style
session notes for the manual history this automates.

## Source repos

- `firecracker_repo` (default: `DecOperations/firecracker-next`) -- a fork of
  `loopholelabs/firecracker` with NATIVE PVM pagetable support built in
  (PVM-aware KVM backend, paravirtualized-guest-compatible pagetable
  handling). Verified directly against its fetched source: `set_tsc_khz()`
  is already a documented no-op for PVM, and the static-CPU-template
  compatibility check is already commented out -- this role no longer
  re-applies hand patches for either (see `tasks/20-firecracker.yml`'s
  comment for the exact verification).
- `pvm_patch_repo` (default: `dywongcloud/pvm-no-fsgsbase-rdtscp`) -- a
  patch series pinned to an **immutable full 40-char sha**
  (`pvm_patch_branch`, currently `415c6a449…`), applied on top of the same
  `virt-pvm/linux` `pvm-612` base (see below). It must be a sha, not a
  branch: `main` gave the checkout no identity, so the "patches applied"
  marker claimed success for whatever `main` happened to be, and
  `pvm_git_update: false` then froze the checkout at first-clone forever
  (live-verified stuck at `14901fef2` on fr/sj2/sp). It must also be the
  FULL sha -- git's wire protocol resolves only full object names, so an
  abbreviated pin fails the clone outright. `pvm_patch_git_update` (default
  **true**, separate from `pvm_git_update`) is what actually moves the
  checkout onto the pin; it is safe to leave on because an immutable ref is
  idempotent by construction.

## The patch series targets TWO different base kernels

This matters more than anything else in this role, and it is why the role
declares `pvm_patch_files` instead of globbing `patches/*.patch`:

| Patch | Base it targets | Applied here |
|---|---|---|
| `0001x86pvmvdsogetcpulslfallback.patch` | `virt-pvm/linux` `pvm-612` | **yes** |
| `pvm6.12to7.1complete.patch` | vanilla `torvalds/linux` **v7.1** | **no** (`pvm_patch_files_excluded`) |

The forward-port patch is not a fix on top of this base -- it *creates* the
PVM tree (`arch/x86/kvm/pvm/*`, `arch/x86/kernel/pvm.c`; 12 `new file mode`
headers) on a **vanilla v7.1** kernel. Verified by blob identity, which is
exact: its `a/` pre-image hashes ARE v7.1's files (`arch/x86/Kconfig`
`f3f7cb01d69d`, `arch/x86/kvm/mmu/mmu.c` `f0144ae8d891`,
`arch/x86/entry/entry_64.S` `42447b1e1dff`), and none of them match
`pvm-612`. `git apply --check` exits **0** against a pristine v7.1 checkout
and produces **276 errors** against `pvm-612`, where GNU patch reports
`Reversed (or previously applied) patch detected` -- because `pvm-612`
already has PVM. Applying it here is a category error, not a mismatch to
force. The patch repo's own notes say the same thing
(`skills/references/runtime-fixes.md`): it "targets a base this repo hasn't
yet moved to". **Moving this role to a v7.1 base is a separate, deliberate
migration, not a patch bump.**

`25-patch-series.yml` asserts that `pvm_patch_files` +
`pvm_patch_files_excluded` account for **every** `*.patch` file in the
series, so a patch appearing or disappearing upstream stops the play with
the filename instead of silently changing what gets built.

Two more things the apply path does deliberately, both measured:

- **GNU `patch --fuzz=3`, not `git apply`.** `git apply` has zero fuzz, and
  maintained `pvm-612` has taken upstream's cpufeatures annotation churn
  (`/* "tdx_guest" … */`) *inside* this patch's context window -- so
  `git apply` rejects all three files of a perfectly correct patch. GNU
  patch at fuzz 3 applies it and lands it in exactly the right place:
  verified by diffing all three resulting files against the series' own
  already-patched reference tree (`kernel/` at `415c6a449`) --
  **byte-identical**. `--3way` is not used either: it is non-atomic and can
  write conflict markers into the tree while returning 1, and a kernel tree
  containing `<<<<<<< ours` does not compile.
- **A failed apply FAILS THE PLAY, and the effect is asserted.** The old
  code ended each apply with `|| echo "skip …"`, so a mismatch was silent
  while the downstream `kvm-pvm.ko` assert still passed -- the role could
  ship an entirely unpatched kernel that looked patched. Now the apply is
  fatal, `X86_FEATURE_PVM_RDTSCP` is asserted present in all three files it
  must reach, and the tree is swept for conflict markers before the ~30
  minute compile.

## FSGSBASE / RDTSCP: no longer a hard requirement

Historically PVM refused to load at all on a host whose vCPU hides
`fsgsbase`/`rdtscp` from guest CPUID (common on cloud VMs, masked for
live-migration compatibility). `pvm_patch_repo`'s series converts both
from hard `-EOPNOTSUPP` failures into **soft runtime fallbacks**: an
MSR-based GS-base switcher path, RDTSCP trap-and-emulation, and a guest
FS-base hypercall fallback. `tasks/00-preflight.yml` checks for both and logs
which path this host will use -- it no longer hard-fails when either is
missing.

**Those two facts are INFORMATIONAL and must never gate work again.** They
used to gate the patch application, and because both flags are in fact
present on 7 of the 8 `fc_pvm` hosts (measured: va, va2, sj2, sp, fr,
cvmsj1, cvmsj2 all expose both; only va3 reads 0/0), the gate silently
skipped the patch series almost fleet-wide -- confirmed by
`.hive_patches_applied` being absent on fr/sj2/sp. The build host's CPU flags
were never the right question: `X86_FEATURE_PVM_RDTSCP` is a **runtime** gate
evaluated inside the built kernel (it is set only when the guest is PVM *and*
the host exposes RDTSCP), so the patch is always wanted and is now applied
unconditionally.

The fallback path is real but has a cost (the patch series' own published
numbers, single Tencent Cloud CVM):

| Workload | Native | PVM fallback | Ratio |
|---|---|---|---|
| CPU compute loop | 0.957s | 1.113s | 1.16x |
| `getpid` syscall | 445ns | 759ns | 1.7x |
| `fork+exec` x3000 | 1.53s | 5.59s | 3.6x |
| `fork` x3000 | 0.81s | 3.48s | 4.3x |
| raw RDTSCP | n/a | ~3.35us (trap/emulated) | -- |

Fork- and syscall-heavy workloads pay real overhead on a fallback-path host.
A host that DOES expose fsgsbase/rdtscp uses the fast native path with no
behavior change. Check which class a given host is in:

```bash
grep -o fsgsbase /proc/cpuinfo   # empty output = fallback path
grep -o rdtscp /proc/cpuinfo     # empty output = fallback path
```

Other requirements: x86_64, RHEL family (Rocky/Alma/RHEL **10** tested), root
via SSH, ~10 GB free disk, internet access. First run compiles two kernels +
Firecracker (~20-40 min on 32 vCPUs).

## The critical PVM-kernel-non-portability gotcha

A PVM guest kernel (`vmlinux`) is built against a SPECIFIC host `kvm_pvm`
ABI and is **not portable across VMs**, even between two VMs that look
identical. Copying a working `vmlinux-pvm` from one PVM host to another
causes the guest to **triple-fault instantly** (`KVM_EXIT_SHUTDOWN`, zero
kernel output) even though the host kernel version string matches. This is
why this role builds the host kernel, guest kernel, AND Firecracker
from source on every target host -- never skip `host_kernel`/`guest_kernel`
by copying artifacts between hosts, no matter how similar they look.

`nokaslr` may be required in `hive_fc_boot_args` (group_vars/fc_pvm.yml) on
some kernel/Firecracker version combinations -- PVM + KASLR can be
incompatible. If a guest triple-faults on an otherwise-correct from-source
build, try adding `nokaslr` before re-diagnosing further.

## What it does (stages / tags)

| Tag | Stage |
|-----|-------|
| `preflight`     | assert x86_64 + RHEL, note fsgsbase/rdtscp presence (informational), make work dirs |
| `packages`      | install build + runtime dependencies |
| `firecracker`   | install Rust, clone the fork (native PVM support, no patches needed), build, install `firecracker`/`jailer` |
| `host_kernel`   | build + install the `pvm-612` host kernel (`CONFIG_KVM_PVM=m`, `pti=off` fixes) |
| `guest_kernel`  | build the PVM guest `vmlinux` (stripped) |
| `rootfs`        | build an Ubuntu ext4 rootfs with an injected SSH key |
| `boot`          | `pti=off`, auto-load `kvm-pvm`, set default kernel, **reboot**, verify `/dev/kvm` |
| `smoke_test`    | boot a microVM, ping + SSH into the guest, assert it's up — **off by default**, and skipped on an already-provisioned host |

## Rolling this out safely

Two safety properties are structural now, not conventions to remember:

- **`site.yml`'s `fc_pvm` play is `serial: 1`.** The role reboots into a
  freshly-built custom kernel; without `serial` the reboot fires on every
  `fc_pvm` host in lockstep, and a kernel that fails to boot takes the whole
  group down at once. A failure on one host stops the roll before the next.
- **`pvm_run_smoke_test` defaults to `false`.** It boots a real microVM,
  which on an unproven PVM kernel **hard-resets the host** (fc-frankfurt, 3
  resets on 2026-07-31 — `docs/blocked-work.md`,
  `docs/pvm-upstream-report.md` §8). It previously defaulted to `true` *and*
  ran outside the `pvm_already_provisioned` guard, so a routine re-run that
  correctly skipped every build step still fired a host-resetting microVM
  boot on a live node. It is now bring-up-only: opt in per host, before the
  node carries traffic.

Hosts are excluded from the roll with the `pvm_skip` host var (inventory), not
by remembering a `--limit`. Currently set on:

| Host | Group | Why |
|---|---|---|
| `fr` (fc-frankfurt) | `fc_pvm` | microVM boot hard-resets the host; `docs/blocked-work.md:76-80` |
| `cvmsj1`, `cvmsj2` | `fc_gpu` | hard-down after a host reset (also not in `fc_pvm` today) |

So the eligible `fc_pvm` set is **`hk, sj, sj2, sp, va, va2, va3`** — `fr` is
in the group but no-ops via `pvm_skip`. To roll to only the eligible hosts:

```bash
cd ansible

# Belt AND braces: --limit excludes fr explicitly, and pvm_skip no-ops it anyway.
ansible-playbook playbooks/site.yml --tags pvm_firecracker \
  --limit 'fc_pvm:!fr'

# Equivalent explicit form, if you prefer naming the hosts:
ansible-playbook playbooks/site.yml --tags pvm_firecracker \
  --limit 'hk,sj,sj2,sp,va,va2,va3'

# One host at a time is already enforced by `serial: 1`, but for a first
# cautious pass do a single host and inspect it before continuing:
ansible-playbook playbooks/site.yml --tags pvm_firecracker --limit va3
```

`va3` is the natural first host: it is the only `fc_pvm` node measuring
`fsgsbase=0 rdtscp=0`, i.e. the one that actually exercises the fallback path
the series exists for.

## Usage

```bash
cd ansible
ansible-playbook playbooks/site.yml --limit 'fc_pvm:!fr' --tags pvm_firecracker

# stage everything but don't reboot (control the reboot yourself)
ansible-playbook playbooks/site.yml --limit 'fc_pvm:!fr' --tags pvm_firecracker \
  -e pvm_reboot=false

# bring-up only, on a node that is NOT yet carrying traffic: prove a real
# microVM boots. This can hard-reset the host -- never run it on a live node.
ansible-playbook playbooks/site.yml --limit <new-node> --tags pvm_firecracker \
  -e pvm_run_smoke_test=true

# re-run only part of it (every expensive step is idempotent via `creates:`)
ansible-playbook playbooks/site.yml --limit 'fc_pvm:!fr' --tags firecracker
ansible-playbook playbooks/site.yml --limit 'fc_pvm:!fr' --tags host_kernel,guest_kernel

# bump the patch series: change pvm_patch_branch to the new full 40-char sha and
# review every patch in it against pvm-612 (the play FAILS until the declared
# pvm_patch_files / pvm_patch_files_excluded lists account for all of them).
# The marker is keyed on the sha, so this genuinely re-applies.
```

## Result

After a successful run the target has, in `/root/pvm/`: `firecracker`,
`jailer`, `vmlinux-pvm`, `rootfs.ext4`, `id_rsa`, `vmconfig.json`,
`run-test.sh`. It boots the PVM kernel by default with `/dev/kvm` provided by
`kvm-pvm`.

## Notes & caveats

- **Custom kernel + reboot.** Have console access to the VM in case the
  kernel fails to boot. `pvm_reboot=false` lets you stage everything and
  reboot yourself. (`grubby`'s `saved_entry` keeps the stock kernel as a
  fallback.)
- **gnu build target.** Firecracker is built for `x86_64-unknown-linux-gnu`,
  which uses an empty seccomp filter (no `libseccomp` at runtime). For a
  hardened musl/static build, build via the upstream `tools/devtool` in a
  container instead.
- **Idempotent.** Safe to re-run; finished steps are skipped (guarded by
  `creates:`). To force a kernel rebuild, remove its `creates:` marker (e.g.
  the `bzImage`) or wipe `build_root`.
- Live-migration support from the upstream fork is intentionally out of
  scope here (not needed to run microVMs without KVM).
- Production hosts in this fleet predate the `firecracker-next` /
  `pvm-no-fsgsbase-rdtscp` source pins this role now uses and may carry
  additional hand-applied hardening patches from that earlier lineage
  (MSR/host-frame-restore fixes on top of a generic fork + manual patches)
  that are not part of either new repo. If a from-source build here produces
  a less stable guest than a hand-tuned production host, that gap is the
  likely cause -- treat this role as the clean, reproducible baseline, not a
  byte-for-byte mirror of every production node's exact patch history.
