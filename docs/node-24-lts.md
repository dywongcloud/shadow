# Node.js 24 LTS is the default for builds and deployments

Status: 2026-11-25. One shared default, PROBED, never assumed. Live
witnesses and operator-gated residuals are listed at the end.

## The one default

`fluid_core` owns three constants and every surface reads them:
`DEFAULT_NODE_MAJOR = 24`, `DEFAULT_NODE_VERSION = "24.22.3"` and
`DEFAULT_BUILD_OUTPUT_NODE_RUNTIME = "nodejs24.x"`. `fluid-core` is a
dependency of both `hive-cloud` and `fluid-gateway`, so the build lane and
the Build Output API v3 admission cannot drift. `fluid-build` does not
depend on `fluid-core`; it emits a generic `"nodejs"` per-route kind, not a
version, so there is no second version there to change.

## What changed

| Surface | Before | Now |
| --- | --- | --- |
| Build Output API v3 runtime | `nodejs20.x / 22.x / 24.x` in one order | default `nodejs24.x` first in `SUPPORTED_BUILD_OUTPUT_NODE_RUNTIMES`; 20/22 stay ADMITTED (no amnesia) |
| Host build PATH | `preferred_node_bin` scanned v20-v24 nvm dirs; `run_host` used ambient PATH | strict `v24.MINOR.PATCH` parse, sha256-verified tarball install first, operator `HIVE_NODE_BIN` override, `run_host` prepends the PROBED bin after checkout shims |
| Cache identity | directory NAME named the Node version | `build_toolchain_identity` executes `node --version`; `verified=0` plus `NODE_RUNTIME_MISSING` when the ACTUAL major is not 24 |
| CI / release | `node-version: 20` | `node-version: 24` in both workflows |
| Host provisioning | dnf `nodejs` (mixed v22 patches) plus a `node:sqlite` gate | exact `v24.22.3` tarball, sha256-pinned, `force: true`, symlinks into `/usr/local/bin`, exact-version `failed_when` (idempotent) |
| UI deploy preflight | `node:sqlite` only (Node >= 22.5) | requires the default major 24 AND `node:sqlite` |
| Rootfs default image | `node:20-slim` | `node:24-slim` (`hive_platform_rootfs/defaults`, `scripts/shadwd.sh`) |
| Docs / examples | `node:20`, `--warm node:20=2` | `node:24-slim`, `--warm node:24-slim=2` |

A directory name is not a version. `strict_lts_dir` refuses `v24.evil`,
`v24`, `v24.0-rc`, a fourth component and every non-24 major;
`node_version_major` independently refuses canary suffixes, `unavailable`,
a missing `v` and a fourth component. The binary must actually REPORT
major 24 before the cache identity is allowed to claim it.

## Operator diagnostic

`hive-cloud --node-probe` (same family as `--dns-probe` /
`--swarm-probe`) binds no port, joins no mesh and starts no node. It
executes the PROBED binary, refuses strict-version impostors, prints the
cache identity, and exits NON-ZERO on NOT CAPABLE. Zero completed executions
cannot print `NODE_PROBE=PASS`. A static boot log
(`boot: default Node.js LTS major selected for builds and deployments`)
is a plain literal, so `strings -a /proc/<MainPID>/exe` can prove the
RUNNING binary after a roll; a Rust format literal cannot.

## HOLD: operator-gated residuals (NOT claimed done)

| Residual | Required gate |
| --- | --- |
| Live Firecracker cells still boot a rootfs baked from `node:20-slim` | `hive_runtime_artifact_rootfs_rebuild` and `_drained` are operator-owned migration flags; a `--tags backend` roll never replaces a booted rootfs. Changing the DEFAULT image does not rebuild `default.ext4`. Needs an explicit drain/rebuild word. |
| Reviewed BuildExecutor base is digest-pinned | A Node-24 reviewed base means a NEW archive/config digest in `build_executor.rs` AND byte-identical `hive_build_executor/vars/main.yml`, a re-review and a roll. This is a supply-chain one-way door, not an ordinary converge. |
| `sandbox-node24.ext4` is not provisioned on the fleet | Sandbox runtimes are separate baked images. Flipping `default_runtime()` / `runtime_image()` / the UI default to `node24` without baking that image on every sandbox-capable node would make EVERY new sandbox fail. Keep the provisioned `node22` default until a bake-and-verify roll. |
| `--tags backend` does not run `prerequisites` | Node 24 on hosts needs `playbooks/site.yml --tags node24` (the narrow tag added to only the Node tasks), then the backend roll. |

The isolated BuildExecutor lane keeps its own sealed `BASE_PATH` inside the
reviewed image; the host lane is the default and is fully covered here.
Nothing advertises an unprobed capability.
