# Vue.js as a supported framework

Status: 2026-11-26. Vue.js is ONE framework preset with TWO build lanes, and
it is detected, selectable, buildable and deployable through the same checked
primitives as every other framework. `hive-cloud --vue-probe` is the live
witness; the backend roll is verified on all six nodes (see "Live roll").

## What was already true

A `vue` preset existed in `fluid_build::PRESETS`, the dashboard had a Vue logo,
a Vue template on `/new`, `GET /v1/build/frameworks` serialized the preset, and
`svcgraph` recognised the `vue` dependency. A project could therefore be Vue
in some narrow cases. It was not supported end to end.

## The gaps that made it unsupported

1. The preset hard-coded `vue-cli-service build`. That binary exists only in a
   Vue CLI project; a Vue 3 + Vite project (the default `create-vue` output)
   declares `vue`, `@vitejs/plugin-vue` and `vite`, and its build script is
   `vite build`. Building a default Vue 3 project with the Vue CLI binary
   fails before producing `dist`.
2. Detection only knew `vue.config.js` and `@vue/cli-service`. A Vue 3 + Vite
   checkout carries `vite.config.*`, so it was reported as generic **Vite**,
   and `vue.config.ts` / `vue.config.mjs` / `vite.config.mts` were invisible.
3. `app_discovery::classify` inserted `vue` for `@vue/cli-service` and `vite`
   for `vite` independently, then refused any app with more than one framework
   as "conflicting framework evidence". A Vue 3 + Vite workspace declares
   BOTH, so deployment selection failed closed on the most common Vue app.
4. `repository_build::metadata_inputs` sealed only `vue.config.js` and
   `vite.config.js` / `vite.config.ts`; edits to the other config flavours
   could serve a stale build snapshot.
5. The dashboard's Framework Preset picker had no Vue.js option, and
   `preset_by_name` refused an explicit `Vue.js` / `vuejs` selection with
   `InvalidFramework`.
6. The landing stack strip labelled the tile "Vue CLI", understating the
   preset.

## What changed

`fluid_core`-style one-default discipline was not needed here: the preset
catalog lives in `fluid_build::framework`, and every surface reads it.

| Surface | Now |
| --- | --- |
| `VUE_DEPENDENCIES`, `VUE_CONFIG_MARKERS`, `VITE_CONFIG_MARKERS`, `vue_present` | ONE shared, PROBED predicate in `fluid_build::framework`. Both build detection and `hive-cloud` app discovery call it. No surface guesses Vue from a directory name. |
| `vue` preset | `build_command: "npm run build"`, `dev_command: "npm run dev"`, `output_dir: "dist"`, `Primitive::Static`. The project's own script is authoritative for Vue CLI and Vue 3 + Vite, and `pmify` rewrites it for pnpm / yarn / bun. |
| `detect_with_package` | Vue evidence is probed once, from dependency names AND config markers. Nuxt config/deps outrank Vue. `vite.config.*` resolves to `vue` when Vue evidence exists, otherwise stays `vite`. All config flavours are covered. |
| `preset_by_name` | Accepts `Vue.js`, `vuejs`, `Vue 3`, `vue3` as aliases for the `vue` slug. |
| `app_discovery::classify` | Uses the shared predicate; removes generic `vite` (and `vue` for Nuxt) before the single-framework assertion, so a Vue 3 + Vite app selects cleanly. |
| `repository_build::metadata_inputs` | Seals every Vue/Vite config flavour. |
| `svcgraph` | `vue`, `@vue/cli-service`, `@vitejs/plugin-vue` all label Vue. |
| Dashboard | Framework picker offers `Vue.js`; framework logo accepts `vuejs` / `vue.js` / `vue3`; `/new` adds a verified Vue 3 + Vite starter from `vitejs/vite` `packages/create-vite/template-vue`; the stack tile says Vue.js. |

## Deployments

`Primitive::Static` gives a catch-all route to `/index.html`, so Vue Router
history mode resolves deep links. A Vue app selected by app discovery is built
with its own script into `dist`, then normalized into Build Output API v3 by
the same `synthesize_config` path as every other static framework.

## Live witness

`./target/debug/hive-cloud --vue-probe` executes the REAL primitives on scratch
repositories (removed by a Drop guard) and exits non-zero unless every one of
these holds:

- Vue CLI checkout (verified against `vercel/vercel` `examples/vue`): slug
  `vue`, `npm run build`, `dist`, Static.
- Vue 3 + Vite checkout (verified against `vitejs/vite`
  `packages/create-vite/template-vue`): slug `vue`, `npm run build`, `dist`,
  Static, not generic Vite.
- Vue 3 + Vite with `pnpm-lock.yaml`: `pnpm run build` (package-manager
  rewrite).
- Plain Vite stays `vite`; Nuxt stays `nuxtjs`.
- `preset_by_name` resolves all four Vue.js spellings; the catalog's `vue`
  preset carries the shared build command.
- Static synthesis has the SPA fallback.
- A Vue 3 + Vite workspace selects one application whose framework
  evidence is `framework:vue` with no `framework:vite`, the previously
  failing deployment case. Other evidence (`static:index.html`) is expected.

## Live roll (2026-11-26)

The backend is live on all six nodes from pushed SHA
`017be7af6fe8f93b2c7fb8454229bc443b25860a`. Every `hive-node.service`
executes the same 150601960-byte artifact, sha256
`c4173192828940ef28e8854496043aaeb7b775bd97a1e96a8f79cdbdeab1b985`,
mtime 04:28 UTC, unit start 04:30-04:37 UTC. `strings -a
/proc/<MainPID>/exe | grep -c 'VUE_PROBE=PASS'` returns 1 on each host.
`/root/fc-target/release/hive-cloud --vue-probe` prints `VUE_PROBE=PASS`
with all nine RESULT lines on fc-virginia and fc-phoenix, and live
`GET /v1/build/frameworks` returns the `vue` preset with `npm run build`,
`dist` and `static`. CI run 45752714094 is success, 11/11 jobs.

No second roll was run: the playbook refuses a run whose artifact matches
the on-disk binary and `/proc/<MainPID>/exe`, and a redundant six-host
serial restart buys nothing.

## HOLD: operator-gated residuals

- The served dashboard build is stale. `/root/hive/ui/.next` (BUILD_ID
  `7S08zWBE_kUO100ORg6j7`) has zero `Vue.js` / `Vue 3 + Vite` occurrences
  in 3274 files on fc-virginia and 3279 on fc-phoenix, so the picker
  option, logo aliases, `/new` starter and corrected landing tile are in
  HEAD but not in the served UI. `--tags backend` never rebuilds the UI.
  A `--tags ui` roll is a production change needing the operator's word,
  and the elected `ui_builder` is the chain head, which has 0 bytes free.
- The chain head fc-sanjose has `/dev/vda2` 2.0T/2.0T, 0 bytes free and
  9731 `No space left on device` journal lines in seven days. 922 litebox
  runtime tars take 308.1 GB, `/var/lib/hive/data` 726.0 GB, `/var/log`
  19.7 GB and `/tmp` 12.5 GB in 700006 files. Nothing was deleted; every
  reclaim needs a blast-radius guard and an explicit operator word.
- The reviewed, digest-pinned BuildExecutor base is unchanged. Its Node
  version is whatever that reviewed image carries; changing it needs a new
  pinned digest plus a roll.
- `sandbox-node24.ext4` is not baked fleet-wide, so the sandbox runtime
  defaults stay on the provisioned `node22`.
- The booted Firecracker `default.ext4` is still built from `node:20-slim`;
  `hive_runtime_artifact_rootfs_rebuild` and `_drained` stay false.
