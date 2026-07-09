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

## The one hard requirement

PVM needs the **`fsgsbase`** CPU feature exposed to the guest vCPU. Many cloud
hosts hide it (for live-migration compatibility), and PVM **cannot** run
there. Check first:

```bash
grep -o fsgsbase /proc/cpuinfo   # must print "fsgsbase"
```

`tasks/00-preflight.yml` asserts this and stops immediately if it's missing.

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
| `preflight`     | assert x86_64 + RHEL + **fsgsbase**, make work dirs |
| `packages`      | install build + runtime dependencies |
| `firecracker`   | install Rust, clone the fork, re-apply the PVM source patches, build, install `firecracker`/`jailer` |
| `host_kernel`   | build + install the `pvm-612` host kernel (`CONFIG_KVM_PVM=m`, `pti=off` fixes) |
| `guest_kernel`  | build the PVM guest `vmlinux` (stripped) |
| `rootfs`        | build an Ubuntu ext4 rootfs with an injected SSH key |
| `boot`          | `pti=off`, auto-load `kvm-pvm`, set default kernel, **reboot**, verify `/dev/kvm` |
| `smoke_test`    | boot a microVM, ping + SSH into the guest, assert it's up |

## Usage

```bash
cd ansible
ansible-playbook playbooks/site.yml --limit fc_pvm --tags pvm_firecracker

# stage everything but don't reboot / test (control the reboot yourself)
ansible-playbook playbooks/site.yml --limit fc_pvm --tags pvm_firecracker \
  -e pvm_reboot=false -e pvm_run_smoke_test=false

# re-run only part of it (every expensive step is idempotent via `creates:`)
ansible-playbook playbooks/site.yml --limit fc_pvm --tags firecracker
ansible-playbook playbooks/site.yml --limit fc_pvm --tags host_kernel,guest_kernel
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
- Production hosts in this fleet may carry additional hand-applied hardening
  patches accumulated across incident response (TSC/MSR/host-frame-restore
  fixes on top of the two PVM forward-port changes this role applies) that
  are not yet folded back into this role. If a from-source build here
  produces a less stable guest than a hand-tuned production host, that gap
  is the likely cause -- treat this role as the clean, reproducible
  baseline, not a byte-for-byte mirror of every production node's exact
  patch history.
