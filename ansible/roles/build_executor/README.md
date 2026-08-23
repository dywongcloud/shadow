# BuildExecutor provisioning role

`build_executor` provisions the host half of the fail-closed
`hive-build-executor/v1` boundary for repository-controlled builds. It publishes
`/var/lib/hive/build-executor.json` only after immutable supply-chain checks,
live gVisor execution, network enforcement, and destructive cleanup probes all
succeed. A failed run revokes the capability under the same root flock used by
executors.

The role does not change Podman's fleet-wide default runtime, run `podman system
migrate`, or provide an alternate runtime. The capability's `podman_path` is a
root-owned wrapper that already supplies the absolute runsc path, every reviewed
runsc flag, the dedicated containers configuration, and an empty hooks directory.
Consumers pass only Podman subcommand arguments; passing `--runtime`,
`--runtime-flag`, configuration/storage overrides, or remote-connection options
is rejected.

## Required inventory inputs

No supply-chain digest, subnet, gateway, or DNS upstream is guessed:

```yaml
build_executor_builder_source: build
build_executor_builder_base_image: "REGISTRY/REVIEWED-BUILDER@sha256:<64 lowercase hex>"
build_executor_network_subnet: "172.31.252.0/24" # syntax example only
build_executor_network_gateway: "172.31.252.1"   # syntax example only
build_executor_dns_upstream_ipv4:
  - "1.1.1.1"
  - "8.8.8.8"
```

gVisor itself needs no operator input: `build_executor_gvisor_release`,
`build_executor_gvisor_archive_sha256`/`_sha512`, and the six-member
`build_executor_gvisor_members` allowlist (each with its own pinned SHA-256)
are role-owned reviewed defaults, the same trust tier as
`build_executor_runsc_runtime_flags` — re-derived and independently
re-verified on 2026-08-22 by downloading the exact pinned release, checking
both its published SHA-512 and an independently computed SHA-256, extracting
it, and re-hashing all six members on a real (non-fleet) Linux x86_64 host.
Override `build_executor_gvisor_release` only to move to a newly reviewed
release, and update the archive/member checksums together with it — never
independently.

`build_executor_fleet_public_ipv4` defaults to every `hive_public_ip` in the
inventory's authoritative `platform` group. The role validates, deduplicates,
and numerically sorts that exact globally-routable IPv4 set. Override it only
when another inventory is authoritative.

The subnet example is not a default. The supplied canonical RFC1918 `/16`
through `/28` is rejected if it overlaps any host route or other Podman network,
or if its bridge name is already externally owned. An existing same-name network
is accepted only when driver, bridge, subnet, gateway, DNS upstream set, IPv6,
internal mode, and canonical policy-ID label all match.

Archive mode accepts only a root-owned checksum-reviewed OCI image-layout v1
archive:

```yaml
build_executor_builder_source: oci_archive
build_executor_builder_oci_archive_path: /root/reviewed-builder.oci.tar
build_executor_builder_oci_archive_sha256: "<64 lowercase hex>"
build_executor_builder_expected_image_id: "sha256:<64 lowercase hex>"
```

Before host mutation, the role verifies the archive checksum and walks its OCI
index/manifest descriptors to prove the expected image config digest is
reachable. It rejects duplicate, malformed, oversized, or digest/size-mismatched
metadata. After load, execution still names only the expected immutable image
ID.

Build mode pulls only the supplied digest, renders this role's reviewed
Containerfile, and builds it with `--pull=never --network=none` through the
hardened runsc wrapper. The base must already contain the full declared toolchain;
the Containerfile never contacts a package repository. Both source modes require
the image to default to the configured numeric non-root uid:gid, declare no OCI
volumes, carry the v1 protocol label, and expose every exact tool path exercised
by the executor.

## Runtime and Podman policy

The role installs gVisor as ONE atomic whole-bundle release, never a
hand-picked binary subset. The upstream release is a single archive
(`gvisor.tar.bz2` + its published `.sha512`) containing exactly six members —
`runsc`, `containerd-shim-runsc-v1`, and four `gvisor-bin/` support binaries
(`checkpointgofer`, `gvisor_sentry`, `gvisor-sentry-prewarmer`,
`runsc-metric-server`). The archive is verified at the archive level (its
published SHA-512 plus an independently pinned SHA-256) before extraction,
then every one of the six members is independently re-verified by exact path
and SHA-256 against `build_executor_gvisor_members`; an archive shaped
differently from the reviewed six-member set — one file more, one fewer, or
any content drift — fails the install outright. The verified tree is
extracted into a never-mutated versioned directory
(`/usr/local/libexec/hive-build-executor/gvisor-releases/<release>/`) and
only made live by atomically swapping the
`/usr/local/libexec/hive-build-executor/gvisor-current` directory symlink
onto it (temp-symlink + same-filesystem rename, one `rename(2)` for the whole
tree). `runsc` is therefore never exposed without the rest of the bundle
alongside it — they always share one release directory and one link — and a
previous release stays on disk (up to `build_executor_gvisor_retain_releases`
generations) so rollback is re-pointing `build_executor_gvisor_release` at a
prior value and re-running the role, never a re-download.

- `/usr/local/libexec/hive-build-executor/gvisor-current/runsc`
- `/usr/local/libexec/hive-build-executor/gvisor-current/containerd-shim-runsc-v1`
- `/usr/local/libexec/hive-build-executor/gvisor-current/gvisor-bin/*`

Verification goes beyond a version string. `runsc --version` must still
contain the configured exact release line, but `containerd-shim-runsc-v1` is
additionally proven by invoking it with no arguments and requiring its real
namespace-validation logic to reject the call (a corrupted or arch-mismatched
binary fails to exec at all rather than emitting that exact refusal), and
each `gvisor-bin` support binary is invoked with `--help` and checked against
the exact exit code and output substring observed live against the pinned
release on a real Linux x86_64 host. `gvisor_sentry` — the binary `runsc`
execs to actually launch a sandbox — is additionally, and more strongly,
proven for real by the live sandboxed `/proc/gvisor/kernel_is_gvisor` boot
later in this same role run: that boot cannot succeed unless `runsc` already
located and exec'd it from the sibling `gvisor-bin/` directory the atomic
switch installed alongside it.

Note that only `gvisor_sentry` is load-bearing for an ordinary sandbox boot
(confirmed live: a sandbox still boots with `gvisor-bin/` entirely absent);
`checkpointgofer`, `gvisor-sentry-prewarmer`, and `runsc-metric-server` back
optional checkpoint/restore, prewarming, and metrics-endpoint features this
role does not otherwise exercise. They are still installed, verified, and
switched atomically as part of the one reviewed bundle — the invariant this
role enforces is bundle integrity and provenance, not that every member is
individually required by every code path.

Every Podman operation uses `/usr/local/libexec/hive-build-executor/podman`,
which starts with an empty environment and globally fixes:

- the absolute runsc binary;
- `platform=systrap`, `network=sandbox`, `gvisor-marker-file=true`, and
  `directfs=false`;
- `host-uds=none`, `host-fifo=none`, `net-raw=false`;
- `allow-flag-override=false`;
- dedicated primary and empty override `containers.conf` files;
- a root-only, freshly emptied OCI hooks directory;
- `env_host=false`, `http_proxy=false`, Netavark, and its nftables driver.

The role asserts the host is already rootful Netavark. It never migrates or
changes a live host's selected backend.

## Network enforcement and restore ordering

The role owns only `table inet hive_build_executor` and
`table bridge hive_build_executor_bridge`; it never edits Netavark's generated
table. Both operator tables are atomically loaded before first network creation.
Their policy-ID comments, exact chains, rules, sets, counters, and destination
members are checked live.

Traffic from the executor bridge is limited to:

1. UDP/TCP DNS to port 53 on the exact network gateway (Aardvark), and
2. IPv4 TCP 80/443 to destinations outside every deny set.

The deny path precedes public web allows and covers loopback, all host-interface
input except gateway DNS, RFC1918, CGNAT, link-local and cloud metadata/platform
endpoints, protocol-assignment/documentation/benchmark ranges, multicast,
reserved space, and every exact fleet public IPv4. All other ports/protocols and
new ingress are dropped. `ct status dnat` drops published-port and hairpin
bypasses before the web allow. Bridge-family `ibrname`/`obrname` filtering drops
same-bridge L2 lateral traffic. IPv6 is disabled in each container and dropped in
both directions by host policy.

`hive-build-executor-firewall.service` preserves `KillMode=process`. On nftables
restart, the BuildExecutor policy restores before the packaged
`netavark-nftables-reload.service`, or a role-owned Netavark 1.16-compatible
fallback. A dedicated nftables drop-in propagates reloads to the firewall
service; its ordered `ExecReload` applies operator policy first, then runs
`podman network reload --all`. If policy application fails, connectivity restore
does not run.

Future `hive-node` starts require the firewall service and run stale cleanup,
atomic policy application, and the checksum-bound live verifier first. Stopping
the firewall stops the requiring hive process, then explicitly reaps labeled
build resources while policy is still active, before nftables can disappear.
No cgroup-wide container kill is used.

## Serialization, cleanup, and probes

A kernel-random UUID identifies every run. A transient root systemd holder takes
an exclusive flock on `/run/lock/hive-build-executor.lock` before capability
revocation or any mutable provisioning step. Image, route/network scan, policy,
network creation, probes, atomic publication, and final verification all remain
inside that exclusive lock. Executor consumers must hold a shared flock on the
same stable root-only inode for every managed session and sealed-output lifecycle;
independent tenant builds may run concurrently while provisioning/reaping waits
for all shared holders.

Startup and next-run cleanup removes only containers and named volumes carrying
one of these labels:

- `io.hive.build-executor.probe=true`
- `io.hive.build-executor.managed=true`

It also removes stale probe listeners/sentinels and root-owned regular,
non-symlink mode-0600 `/tmp/.hive-build-env-*` files only after a one-hour grace;
an unexpected file type fails closed. Every ordinary exit must still explicitly
run `podman rm --force --volumes` and remove its named volume. This preserves the
fleet's load-bearing `KillMode=process` and Podman lock-pool rules.

Before publication, two exact-image/runsc containers and a byte-capped named
tmpfs workspace prove:

- numeric non-root identity, read-only rootfs, private IPC/PID/UTS/cgroup
  namespaces, zero effective capabilities, no-new-privileges, and exact conmon
  runtime path/flags;
- `/proc/gvisor/kernel_is_gvisor` contains gVisor's exact `gvisor` marker;
- no inherited proxy/host sentinel environment, host file/FIFO/Unix socket,
  host IPC sentinel, or dangerous host devices;
- kernel-enforced workspace exhaustion and a real nested rootless Buildah OCI
  archive;
- gateway-only resolvers, no IPv6, and denied direct UDP and TCP port-53 bypass;
- live host gateway/private/metadata/fleet/DNAT-hairpin denial;
- same-bridge peer denial through a separately running exact-address listener;
- counter increases for every specific nft denial path;
- TLS-verified public registry HTTPS returning 200 or registry-standard 401.

The `always` path explicitly removes both containers and the named volume, stops
exact-address listeners, removes every sentinel, and requires all three Podman
objects to be absent. Any cleanup failure prevents capability publication.

## Capability contract

The atomically published root:root mode-0600 JSON contains protocol, immutable
builder image ID, runsc path/version/checksums and fixed flags, wrapper and both
containers-config paths/checksums, empty hooks path, network name/bridge/subnet/
gateway/DNS and canonical policy ID, IPv6/no-fallback declarations, shared lock
path, policy path/digest/tables, exact fleet set, numeric uid/gid, workspace byte
ceiling, and the root-owned live-verifier path plus its SHA-256.

A consumer must reject an absent/unknown/malformed declaration, validate its
ownership and exact schema, hash every pinned file, invoke the checksum-bound
`network_verify_path` before advertising capability, and use `podman_path`
without adding runtime/config flags. It must use only the immutable image ID,
create a uniquely named byte-capped local tmpfs volume (never a host bind), apply
non-root/cap-drop/no-new-privileges/read-only/resource limits, attach only the
recorded network, label every container and volume `managed=true`, share the
provisioning flock, and perform cancellation-safe explicit cleanup. Protocol v1
must refuse Dockerfile/Compose surfaces rather than run a host frontend or
fallback runtime.

Public TCP 443 can carry an application-layer tunnel. The reviewed image and
coordinator-authored command surface remain part of the security boundary; no
host credential, proxy, socket, or secret environment may cross it.

## Playbook integration required (not changed here)

Add a dedicated `[build_executors]` inventory group and its immutable pins,
subnet/gateway, and DNS upstream variables. Add a `become: true`, `serial: 1`
`build_executor` play to `ansible/playbooks/site.yml` after prerequisites and
before `hive_platform`. Canary one idle build host first: the role installs
nftables/systemd ordering and deliberately blocks future `hive-node` starts when
capability or live policy verification fails.

Do not add this role to `ansible/playbooks/platform-only.yml`; that binary-only
fast path must not rebuild a reviewed image or reconverge host firewall state.
No existing inventory, playbook, or role is modified by this role directory.
