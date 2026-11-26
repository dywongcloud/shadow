# Node.js 24 LTS is the default for builds and deployments

Status: 2026-11-26. One shared default, PROBED, never assumed. The backend
roll is live on all six nodes; live witnesses and operator-gated residuals
are listed at the end.

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
| Version measurement | nothing executed the probed binary | `--node-probe` executes `node --version` and refuses PASS when the ACTUAL major is not 24. The `compute_cache_key` helper has only `#[cfg(test)]` callers in this tree, so NO production cache behaviour is claimed here |
| CI / release | `node-version: 20` | `node-version: 24` in both workflows |
| Host provisioning | dnf `nodejs` (mixed v22 patches) plus a `node:sqlite` gate | exact `v24.22.3` tarball, sha256-pinned, `force: true`, symlinks into `/usr/local/bin`, exact-version `failed_when` (idempotent) |
| UI deploy preflight | `node:sqlite` only (Node >= 22.5) | requires the default major 24 AND `node:sqlite` |
| Rootfs default image | `node:20-slim` | `node:24-slim` (`hive_platform_rootfs/defaults`, `scripts/shadwd.sh`) |
| Docs / examples | `node:20`, `--warm node:20=2` | `node:24-slim`, `--warm node:24-slim=2` |

A directory name is not a version. `strict_lts_dir` refuses `v24.evil`,
`v24`, `v24.0-rc`, a fourth component and every non-24 major;
`node_version_major` independently refuses canary suffixes, `unavailable`,
a missing `v` and a fourth component. The binary must actually REPORT
major 24 before the operator diagnostic is allowed to print PASS.

## Operator diagnostic

`hive-cloud --node-probe` (same family as `--dns-probe` /
`--swarm-probe`) binds no port, joins no mesh and starts no node. It
executes the PROBED binary, refuses strict-version impostors, prints the
cache identity, and exits NON-ZERO on NOT CAPABLE. Zero completed executions
cannot print `NODE_PROBE=PASS`. A static boot log
(`boot: default Node.js LTS major selected for builds and deployments`)
is a plain literal, so `strings -a /proc/<MainPID>/exe` can prove the
RUNNING binary after a roll; a Rust format literal cannot.

## ROLL STATUS: ROLLED (2026-11-26 04:28-04:37 UTC)

The pre-roll audit (`scripts/audit-secret-keys.sh`) is clean on all six
hosts. fc-sanjose was a FROZEN/REBOOTING host, not a firewall or sshd
change: three timed connects read `SSH-2.0-OpenSSH_9.9` in 65-78 ms and
both full `ssh` runs exited 0. The earlier `Not allowed at this time`
pre-banner is sshd's listener drop (MaxStartups / PerSourcePenalties).
No sshd, firewall or inventory file was changed by this work.

All six `hive-node.service` units execute ONE identical artifact:
sha256 `c4173192828940ef28e8854496043aaeb7b775bd97a1e96a8f79cdbdeab1b985`,
150601960 bytes, mtime 2026-11-26 04:28 UTC, unit start 04:30-04:37 UTC,
all `active`. `strings -a /proc/<MainPID>/exe | grep -c` returns 1 for the
boot literal and 1 for `VUE_PROBE=PASS` on each of the six hosts. The
fc-virginia journal records the boot message with
`major=24 version="24.22.3"` at 04:30:34 UTC. CI run 45752714094 is
success, 11/11 jobs, on pushed SHA
`017be7af6fe8f93b2c7fb8454229bc443b25860a`.

No second roll was run. The backend roll verifies its own artifact
against the on-disk binary and `/proc/<MainPID>/exe`, and refuses a run
that changes no host's binary. Re-running it now would be a refused
no-op or a redundant six-host serial restart. The roll is verified live,
not re-applied.

Live execution of the DEPLOYED binary on fc-virginia and fc-phoenix:
`--node-probe` prints `NODE_PROBE=PASS` with
`executed=PASS reported=v24.22.3`, `strict_dir=PASS`,
`version_parse=PASS`, `cache_identity=PASS verified=true` and
`node_sqlite=PASS`; `--vue-probe` prints `VUE_PROBE=PASS` with all nine
RESULT lines. Live `GET /v1/build/frameworks` returns 15 presets
including the `vue` preset with `npm run build`, `dist` and `static`.

## HOLD: chain head out of disk (operator one-way door)

fc-sanjose `/dev/vda2` is 2.0T used of 2.0T, 0 bytes free, 100% used;
the last seven days of journal contain 9731 `No space left on device`
lines. Measured occupancy: `/var/lib/hive/data` 726.0 GB, 922 litebox
runtime tars 308.1 GB, `/var/log` 19.7 GB (five rotated `messages-*`
files of 2.1-3.9 GB each), `/tmp` 12.5 GB in 700006 files,
`/root/fc-target` 14.2 GB. A full rootfs with 9731 ENOSPC lines is the
concrete condition behind the intermittent freezes; the earlier
`Not allowed at this time` was sshd's listener drop on a wedged/rebooting
host, not a firewall or sshd config change. Nothing was deleted: a
reclaim needs a blast-radius guard (`gc_rootfs_images` keep-set and
`HIVE_GC_MAX_REAP_FRACTION`) and an explicit operator word. A UI build
cannot succeed here: `parallel-deploy.yml` elects the control-plane
leader (the chain head) as `ui_builder`.

## HOLD: dashboard UI build does not contain Vue

`/root/hive/ui/.next` (BUILD_ID `7S08zWBE_kUO100ORg6j7`, newest file
2026-11-25T15:44:32Z on fc-virginia and 2026-11-25T20:50:23Z on
fc-phoenix) has zero `Vue.js`, `Vue 3 + Vite` or `vuejs` occurrences
across its 3274/3279 files. The Vue picker, logo aliases, `/new`
starter and landing tile are in HEAD but not in the served build;
`--tags backend` never rebuilds the UI, and no gitops deploy newer
than 03:50 UTC on 2026-11-26 exists on fc-virginia. The backend
catalog, detection, plan and deployment selection are live and
witnessed; the dashboard roll is HELD until the disk reclaim is
authorized.

## HOLD: operator-gated residuals (NOT claimed done)

| Residual | Required gate |
| --- | --- |
| Live Firecracker cells still boot a rootfs baked from `node:20-slim` | `hive_runtime_artifact_rootfs_rebuild` and `_drained` are operator-owned migration flags; a `--tags backend` roll never replaces a booted rootfs. Changing the DEFAULT image does not rebuild `default.ext4`. Needs an explicit drain/rebuild word. |
| Reviewed BuildExecutor base is digest-pinned | A Node-24 reviewed base means a NEW archive/config digest in `build_executor.rs` AND byte-identical `hive_build_executor/vars/main.yml`, a re-review and a roll. This is a supply-chain one-way door, not an ordinary converge. |
| `sandbox-node24.ext4` is not provisioned on the fleet | Sandbox runtimes are separate baked images. Flipping `default_runtime()` / `runtime_image()` / the UI default to `node24` without baking that image on every sandbox-capable node would make EVERY new sandbox fail. Keep the provisioned `node22` default until a bake-and-verify roll. |
| `--tags backend` does not run `prerequisites` | All six hosts now carry `/usr/local/node-v24.22.3-linux-x64/bin/node` reporting `v24.22.3`, and `hive-node.service` puts `/usr/local/bin` first in its PATH; `--node-probe` executes that binary on the deployed artifact. An interactive root shell still reports v22.23.0 on fc-virginia-3 (nvm) and v22.23.2 under `sudo -n` on fc-orlando. `preferred_node_bin` probes the strict versioned dir, so build placement is unaffected. Converging the interactive shell default is cosmetic and is not claimed done. |

The isolated BuildExecutor lane keeps its own sealed `BASE_PATH` inside the
reviewed image; the host lane is the default and is fully covered here.
Nothing advertises an unprobed capability.
