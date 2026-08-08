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
