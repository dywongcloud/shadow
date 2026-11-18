# laboon as a container substrate: measured assessment

Recorded 2026-11-17 from live execution and source reads. This is a decision
record, not an implementation. No production code, no roll and no push were
performed. The request asked for laboon to replace podman as the primary
container runtime substrate with functional volumes and networking; the
measurements below explain why that change was HELD rather than made.

## What was examined

| artefact | identity |
| --- | --- |
| upstream repository | `https://github.com/samyfodil/laboon.git`, main `5fb67dbeff804bd40e4633554e98c85e4e4f90c7` |
| release | `v0.1.0`, published `2026-11-17T15:25:44Z` (the same day as this assessment) |
| repository metadata | created `2026-10-23T20:02:05Z`, 1 star, 1 fork, 0 open issues, Go, Apache-2.0, 3427 KB |
| executable witness | `laboon_0.1.0_darwin_arm64.tar.gz`, sha256 `1116d234f49b3f5f3c7ce363ddb98c4d084a4e6d11342a613386a4d89203f279`, verified byte-for-byte against upstream `checksums.txt` before running |
| local clone (read only) | `/private/tmp/laboon-src-20261117`, 541 files |
| adversarial swarm | workflow `wf_8eefe806-600`: 6 recon agents, 3 derived claims, 9 refuters, 1 completeness critic |
| target codebase | this tree at `8feb269d`, with `AGENTS.md`, `ansible/AGENTS.md`, `crates/hive-backend/src/container_cli.rs`, `crates/hive-backend/src/lib.rs`, `crates/hive-cloud/src/schedule.rs`, `crates/hive-edge/src/region.rs` |

## Architecture, as upstream describes it

laboon runs unmodified Linux ELF binaries and OCI images inside a host process.
A Zig stub rewrites syscall instructions to trap into a Go supervisor that
emulates the Linux ABI and owns file and network I/O through one confined `vfs`
Namespace. Guest machine code executes natively on the host CPU, so the CPU
architecture must match (`amd64` or `arm64`) with no translation. The second
isolation layer is the host OS sandbox (seccomp on Linux, Seatbelt on macOS,
AppContainer on Windows) as a kill-on-escape backstop. Guest and enforcement
share one address space; `AGENTS.md` in the laboon tree (Rule 3) and
`README.md:182-196` say this explicitly. This is the same isolation class as
this project's Litebox backend, which `AGENTS.md:116-120` already says is NOT
Firecracker or gVisor isolation and must never be substituted silently for
Firecracker capability.

Upstream states its own maturity limit in `README.md:32-35`:

> laboon is newer, with a small host surface by design; its daemon and OCI
> runtime should not be read as claims of production maturity or complete
> engine compatibility.

`README.md:154-159` says the tested workload table is exercised workloads, not
a promise that every Linux program or syscall is supported. `README.md:225`
says the performance numbers are recorded measurements, not fresh validation.

## Integration surfaces: exactly what exists

| surface | what the code actually implements | source |
| --- | --- | --- |
| `laboon` CLI | `version`, `build` (Dockerfile to image tar), `-image`, `-root`, `-v HOST:GUEST` bind mounts, `-net`, `-no-net`, `-allow-domains`, `-allow-cidrs`, `-allow-ports`, `-env`, `-user`, `-mem`, `-trace` | `cmd/laboon/main.go:104-240` |
| Go library | `WithMount`/`WithMountReadOnly` (host directories only, no named volume store), `WithNet` (outbound egress through supervisor-proxied host sockets), `WithNoNet`, `WithMaxMemory`, `WithUser`, `WithDevice` | `options.go:120-289` |
| `laboond` Docker API daemon | 12 routes: ping, version, info, images/create, containers/create, list, inspect, start, wait, attach, delete | `cmd/laboond/daemon.go:36-51` |
| `laboon-runtime` OCI runtime | subset of `config.json`: `process`, `root`, `hostname`, bind `mounts`; run/create/start/state/kill/delete/pause/resume/exec/features | `cmd/laboon-runtime/spec.go:9-17`, `main.go:42-72` |
| `laboon-mcp` | MCP `run` tool over stdio or streamable HTTP | `cmd/laboon-mcp/main.go` |

Critical gaps in those surfaces:

- `laboond`'s `createBody` has no `HostConfig`, `NetworkingConfig`, `Volumes`,
  `Binds`, `PortBindings`, `Labels` or `RestartPolicy`
  (`cmd/laboond/handlers.go:28-38`). Docker clients send those fields; the
  `json.Decoder` silently discards them. `container.run` never calls
  `WithMount` and never publishes a port (`cmd/laboond/container.go:73-103`).
  `imageCreate` is a fake two-line progress stream with no pull, auth, digest
  or layers (`handlers.go:11-26`). State is one in-memory map with no
  persistence, no restart recovery, no TLS, no auth and no tenant scoping
  (`daemon.go:20-26`). Force-delete drops the map entry but leaves the run
  alive; concurrent attach can double-close `c.attached` and panic.
- There are no `/volumes/*`, `/networks/*`, `/build`, `/exec`, `/logs`,
  `/stop`, `/kill`, `/restart`, `/stats`, `/top`, `/cp`, `/events`, `/images/json`
  routes. `inspect` returns no `HostConfig`, `Mounts`, `NetworkSettings`,
  `Ports` or `Health` (`handlers.go:173-200`).
- `laboon-runtime` maps only `Type == "bind"` mounts, drops `ro`,
  `rbind` and propagation options, ignores `root.readonly`, and never calls
  `WithNet`, so an engine container is loopback-only
  (`cmd/laboon-runtime/run.go:23-26`, `spec.go:33-43`). There is no `linux`
  section: no namespaces, cgroups, devices, hooks, annotations, rlimits,
  capabilities, seccomp, maskedPaths or resources. `features` advertises
  `'hooks':[]` with no cgroup fields (`lifecycle.go:301-310`). `exec.fifo`
  lives in `/run/laboon-runtime/<id>`, not in the bundle as runc-compatible
  engines expect (`lifecycle.go:117-118`). `run --detach`, `--pid-file` and
  `--console-socket` are accepted and ignored (`run.go:84-89`). `kill --all`
  is filtered and the guest leader `GuestPid` is never killed
  (`lifecycle.go:250-295`), leaving an orphan.
- Inbound TCP port publishing is not part of the CLI, library, daemon or OCI
  runtime API. It exists only as `internal/sandbox/lx_portpub.go`, which reads
  nftables DNAT rules that a **nested podman** installs and relays a real host
  port into the internal vnet (`VNET.md:96-104`). It needs podman's
  `enable_port_reservation=false`, and the `containers.conf.d` drop-in does not
  ship in the repository (verified: no such file in 541 files). The `VNET.md:86`
  reference to `lx_netavark.go` names a file that is absent from the tree
  (verified). `VNET.md:106-118` lists UDP publishing, unconnected UDP egress,
  container-name DNS, multiple named networks and a cross-run broker as
  remaining work. `VNET.md:3` says "design + Phase 1 in progress" and
  `:120-124` names the biggest risk as "an incomplete boundary that silently
  reaches existing host/DNS paths".
- Only `laboon` and `laboon-mcp` ship as release binaries
  (`.goreleaser.yaml:16-47`, `.github/workflows/ci.yml:107-108`). `laboond`
  and `laboon-runtime` are vet-only in CI (`:110-114`); an engine-compatible
  daemon or OCI runtime would have to be built from source. Source builds
  require Go 1.27.1 (`go.mod:3`, `go.work:1`) and Zig 0.13.0 for stubs; this
  host has Go 1.26.1. Releases carry only `checksums.txt`: no SBOM, no sigstore
  provenance, no signing. CI tests only `internal/{rewrite,vfs,oci}` (24 of
  128 test files), has no race or fuzz gate, and 6 of 20 recorded runs failed.
  `SECURITY.md:12-15` promises only "aim to acknowledge within a few days".

## Live witnesses (executed, not inferred)

All runs used the checksum-verified `laboon` 0.1.0 darwin/arm64 binary at
`/private/tmp/laboon-bin-20261117/extract/laboon`, with a scratch directory at
`/private/tmp/laboon-witness-20261117`. Commands were passed as argv lists,
never a shell string.

| check | command shape | observed result |
| --- | --- | --- |
| binary identity | `laboon version` | `laboon 0.1.0`, exit 0 |
| OCI image run | `-image alpine:3.20 cat /etc/os-release` | exit 0, `ID=alpine`, `VERSION_ID=3.20.10` |
| volume read | `-image alpine:3.20 -v <dir>:/mnt cat /mnt/host_marker.txt` | exit 0, `HOST_MARKER_7f3a` |
| volume write | `-image alpine:3.20 -v <dir>:/mnt sh -c 'printf GUEST_WROTE_9c21 > /mnt/guest_out.txt'` | exit 0; host read back `GUEST_WROTE_9c21` |
| volume persistence | two separate runs sharing one host dir | run 1 wrote `1`, host saw `1`, run 2 read `1` |
| unrestricted egress | `-image alpine:3.20 -net sh -c 'apk add --no-cache podman'` | exit 0, `OK: 99 MiB in 71 packages` |
| allowlisted egress | `-net -allow-domains dl-cdn.alpinelinux.org -allow-ports 443` plus `wget` | `NET_FAIL`, `wget: can't connect to remote host: Host is unreachable` |
| guest to host loopback | `-net -allow-cidrs 127.0.0.1/8 -allow-ports <p>` plus `nc 127.0.0.1 <p>` | exit 1; host listener received nothing |
| guest listener, host connects | `-net` plus `nc -l -p 45123` in the guest | host connected to `127.0.0.1:45123` and received `GUEST_LISTENER_OK`; repeated with `BOUND` |
| no network by default | `sh -c 'echo X \| nc 127.0.0.1 <p>'` without `-net` | exit 1, refused as documented |
| nested podman | `-image alpine:3.20 -net -v <dir>:/mnt sh -c 'apk add --no-cache podman >/dev/null 2>&1; podman --version'` | `apk add` succeeded; `podman --version` killed the Go supervisor: `fatal error: traceback did not unwind completely`, exit 139 on one run and exit 2 on another; reproduced twice |

Two findings matter for the requested topology. First, `-v HOST:GUEST` bind
mounts and cross-run persistence DO work, so volumes are functional in the
bind-mount sense; there is no named-volume lifecycle. Second, with `-net` a
guest `bind` becomes a real host loopback listener that the host can reach,
while allowlisted egress and guest-to-host loopback both failed. Networking is
therefore partially functional, asymmetric, and proxied through host sockets
rather than an isolated namespace. The operator's explicitly permitted fallback
("it can run podman inside laboon") crashed the supervisor on this host. That
crash is specific to darwin/arm64 and is NOT proven for the fleet's
linux/amd64 glibc 2.39 hosts; it does mean the fallback is unproven on the
fleet, not proven broken there.

## Why the swap was HELD against this codebase's contracts

| contract | anchor | what a laboon swap breaks |
| --- | --- | --- |
| `KillMode=process` is load-bearing; cells outlive their process; every boot reaps its own previous boot's cells; ownership is owner plus boot via `hive.owner`/`hive.boot` | `AGENTS.md:100-108` | `laboond` create has no label field, so `hive_backend::reap_orphaned_cells` cannot identify a laboon container; `laboon-runtime` never kills `GuestPid` |
| every container removal is `rm -f -v`; podman takes one lock from a fixed 2048-lock pool per container AND per volume | `AGENTS.md:100-104`, `container_cli.rs:45-53` | laboon has no volume store and no `rm -v`; `laboon-runtime` maps only bind mounts |
| never `podman volume prune`; reclaim is gated on `is_anonymous_volume` (64 ascii-hex) AND `dangling=true` | `AGENTS.md:100-104` | no laboon equivalent exists to gate |
| podman `short-name-mode=enforcing`, so tenant `FROM` refs must be qualified | `AGENTS.md:112-114` | laboon's OCI pull is a separate path with its own resolution |
| a tenant string never becomes a host path: `-v <a>:<b>` is a bind mount whenever `<a>` looks like a path, so the left side is always a platform-templated NAME (`container_volume_cfg`) | `AGENTS.md:197-200` | `laboon -v HOST:GUEST` REQUIRES a host path; a naive port would let tenant input become a host path |
| tenant processes get their environment from nothing (`env_clear()` in every backend); node env holds `HIVE_SECRET_KEY`, `HIVE_JWT_SECRET`, `HIVE_INTERNAL_TOKEN` | `AGENTS.md:204-208` | `laboond` passes `createBody.Env` straight through with no `env_clear` |
| `--security-opt no-new-privileges` and `--pids-limit` on every podman run | `container_cli.rs:83-108` | `laboond` drops `HostConfig`; `laboon-runtime` has no cgroup or seccomp spec handling |
| managed SQLite lives at `$HIVE_DATA/sqlite-dbs/hive-sqlite-{sanitize_tag(db_id)}.db`; browser CRR at `$HIVE_DATA/browser-dbs/`; `db_gateway` splices on `host_node` with `AuthProof` deadlines; stranded databases are reported, never re-hosted, because re-provisioning yields an EMPTY database | `AGENTS.md:175-189` | moving databases into a confined vfs hides the host-absolute paths and the CRR `ChangeBatch` replication; it is a one-way door |
| capability is PROBED and ADVERTISED, never assumed; `None` means NOT CAPABLE; Litebox needs a passing `--litebox-probe` AND `litebox_verified=true` on `hosts.ini`, never macOS | `AGENTS.md:113-120`, `resources.rs`, `region.rs:95-133` | a new backend needs a new staged runner per glibc group, a probe, a `NodeInfo` field and a `RuntimeCapabilitySource` variant; `resources.rs` is contract-bound (`system.rust.capability`), so that change drifts `check-contract-bindings.py --check` |
| placement gates through ONE predicate shared by `place`, lease-stickiness and `dispatch_fallbacks`; a new backend string must be added to every schedule predicate or placement refuses it | `schedule.rs:33-37`, `schedule.rs:313-348` | `eligible()` currently accepts `firecracker` or `litebox` plus `runtime_artifact_capable`; a laboon backend would be ineligible everywhere or would have to be declared isolated |
| isolation hierarchy is Firecracker then Litebox then Mock, and Litebox is explicitly NOT a hardware isolation boundary | `AGENTS.md:116-120` | making a shared-address-space, seccomp-backstop sandbox the PRIMARY substrate for live tenants weakens a security gate |
| a roll is a production change: operator's word first, read-only `scripts/audit-secret-keys.sh`, a staged binary per glibc group, and a running-binary proof via `strings -a /proc/<MainPID>/exe` (never a Rust format literal) | `AGENTS.md:26-32`, `ansible/AGENTS.md:15-24` | the current request contains no roll word; a `git push` auto-deploys through the webhook and `git::spawn_git_poll_reconcile` triggers, so a push IS a production change |

## Adversarial swarm verdict

The swarm ran 6 independent recon agents (laboon OCI runtime, laboond daemon,
CLI/library volume and network surface, upstream maturity, this tree's
container CLI, this tree's backends and managed data lanes), one deriver, 9
refuters across three lenses (correctness, security, roll gate) and one
completeness critic. Result:

| claim | refutations | verdict |
| --- | --- | --- |
| C1 probe-only, capability-advertised laboon backend | 3 of 3 REFUTED | a false verified flag would let `schedule.rs:33` treat a shared-address-space sandbox as isolated; no staged binary, no running-binary proof |
| C2 swap `container_cli::bin()` from podman to laboond | 3 of 3 REFUTED | laboond is an HTTP daemon with no `HostConfig`, no volumes, no `-p`, no `exec`/`logs`, no auth; breaks `rm -f -v`, `env_clear` and `no-new-privileges` |
| C3 nested podman inside laboon for new workloads | 3 of 3 REFUTED | `createBody` drops binds and ports; inbound publishing needs an unshipped `containers.conf.d` drop-in and nftables DNAT; UDP/DNS/multi-net gaps; volume and database migration is one-way |

`allow_list=[]`, global finding `INFEASIBLE`. 9 of 9 refuter votes were
REFUTED; no claim survived. The critic named 7 unexamined surfaces. Two were
closed by this assessment with direct reads and live execution: nested
podman execution (witnessed above) and scheduler placement (`schedule.rs:33`
and `:313-348` read directly). The remaining five are genuinely open:
production roll and operator consent; live fleet state and currently running
podman workloads; the keep-or-migrate decision for existing workloads;
CI/acceptance and rollback; and formal invariant or type-level verification.

One process defect is recorded honestly. The first swarm run passed an async
thunk as a `pipeline` stage, which returned functions instead of results:
`claims=0`, yet the critic still emitted a plausible `REFUTED x3` table from
an empty evidence table. The stage was fixed at source to
`pipeline(derived.claims, async (c) => {...})` and the run was resumed with
`resumeFromRunId: wf_8eefe806-600`; only the second run's 9 real votes are
cited above. The first verdict was discarded, not reused.

## What would have to be true to proceed

Recorded as PRD row `.gm/prd/laboon-primary-substrate-2026-11-17.yml`, not as
a stub or a partial build. In order:

1. A reviewed, digest-pinned laboon build for each fleet glibc group
   (2.39 amd64), with `laboond` and `laboon-runtime` built from source under a
   pinned Go 1.27.1 toolchain, provenance, and a `--laboon-probe` smoke test
   that exercises volumes, inbound and outbound networking, and nested podman
   on a real linux/amd64 host.
2. A volume and network contract that satisfies `container_volume_cfg`
   (platform-templated names, never tenant strings as host paths),
   `env_clear()`, `rm -f -v` lock reclaim, `no-new-privileges`, and
   `hive.owner`/`hive.boot` labels so orphan reaping still works.
3. A probed and advertised capability in `NodeInfo` and
   `RuntimeCapabilitySource`, with `None` meaning NOT CAPABLE, plus the ONE
   shared placement predicate used by `place`, lease-stickiness and
   `dispatch_fallbacks`. `resources.rs` is contract-bound, so the bindings
   manifest must be regenerated with `--sync` locally (never in CI).
4. A keep-or-migrate decision for managed SQLite and browser CRR databases,
   with proof that no database is re-provisioned empty. Existing podman
   workloads can stay on podman; new workloads can be scheduled inside laboon
   only on nodes that pass the probe.
5. `cargo test --workspace --locked` RUN, `scripts/cli-acceptance.sh`,
   `scripts/check-contract-bindings.py --check`, and a live two-node witness.
6. An explicit operator word for the roll, a passing
   `scripts/audit-secret-keys.sh`, and a running-binary proof.

Until those hold, podman remains the primary container runtime. No security
gate was weakened, no workload was migrated, nothing was pushed and nothing
was rolled.

## Reproducing the witnesses on a fleet host

The exact command shapes are in the "Live witnesses" table. Use the
linux/amd64 release asset from `v0.1.0`, verify its sha256 against the
`checksums.txt` line
`842fe5620db4592498ff599a080c9f8f31b710a27face7ac2ffa13799d5312c8`, keep the
scratch volume directory under a platform-templated name, and pass argv as a
list. Do not run a nested-podman probe against a host carrying live traffic.
Darwin `mktemp` ignores `TMPDIR`; use `/private/tmp` on macOS.
