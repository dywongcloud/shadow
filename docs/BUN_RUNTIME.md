# Bun runtime support

Bun is a first-class deployment/runtime target, orthogonal to (never implied
by) Bun-as-a-package-manager. This document is the compatibility matrix and
the honest record of what was verified versus what is inherited "for free"
from the platform's existing runtime-agnostic design versus what remains a
real, documented limitation.

## Why so little code changed

Before this work, `CellBackend` (the trait both the mock and Firecracker
backends implement), the placement scheduler (`hive-cloud/src/schedule.rs`),
the Fluid concurrency/autoscaler (`fluid-compute`), the runtime cache, and the
native workflow engine were **already 100% runtime-agnostic** — they operate
on `argv`/`CellSpec`/`FunctionLaunch` and exec whatever binary `start_cmd[0]`
names, with zero string-matching on "node" anywhere in that machinery. The
real gaps were narrower and more specific:

1. **No structured runtime signal.** `start_cmd` was a raw argv with no tag
   saying what it was. Four independent copies of `is_node_start_cmd`-style
   basename-sniffing existed across the codebase, and one of them silently
   mis-treated `bun` as V8-compile-cache-eligible (harmless but useless — Bun
   never reads `NODE_COMPILE_CACHE`).
2. **Build-time command synthesis hardcoded `"node"`.** `detect_start_cmd`,
   `adapter_manifest` (OpenNext/vinext), and the SvelteKit branch always chose
   Node regardless of what package manager was detected.
3. **No orthogonal runtime-selection axis.** `bun.lock` presence already
   selected `bun install` (package manager), but nothing let a user or config
   explicitly choose the Bun *runtime* separately.
4. **Bun's bytecode cache is structurally different from Node's**, so it
   needed real, new code, not a copy-paste of the Node path.

Fixing those four things — plus adding Corepack `packageManager` precedence to
package-manager detection — gives Bun the same reach as Node through every
subsystem that was already generic.

## Runtime resolution (the orthogonality contract)

`hive_core::Runtime` (`crates/hive-core/src/proto.rs`) is the single source of
truth, replacing the four duplicated basename-sniffing helpers. Precedence,
implemented in `hive-cloud/src/git.rs::build_via_fdi`:

1. `vercel.json` `{"runtime": "bun"}` (or `"nodejs"`) — the platform-native,
   explicit selector.
2. `vercel.json` `{"bunVersion": "1.x"}` — Vercel's own Bun-beta selector
   (major-version-only; presence alone means "use Bun").
3. Project Settings' explicit `runtime` field (`BuildConfig.runtime`).
4. Otherwise: infer from the resolved start command's basename
   (`Runtime::infer_from_argv`) — **today's exact behavior**, so every
   existing Node deployment, and every Bun-*package-manager*-only project
   (a `bun.lock` with no explicit runtime choice), keeps running on Node
   exactly as before. A `bun.lock` alone never forces the runtime.

Package-manager detection (`fluid_build::detect_package_manager`) is a
**completely separate** function with its own precedence:
`package.json#packageManager` (Corepack) > `bun.lock` > `bun.lockb` >
`pnpm-lock.yaml` > `yarn.lock` > `package-lock.json` > default (npm). It never
reads or writes anything runtime-related. A conflicting lockfile (e.g. both
`bun.lock` and `pnpm-lock.yaml` committed) is never deleted or silently
dropped — the winner is deterministic and a warning naming the losing
lockfile(s) is logged and returned in `PackageManagerDetection.conflict_warning`.

## Compatibility matrix

| Feature | Node.js status | Bun status | Implementation path | Tests | Known limitations | User-visible behavior |
|---|---|---|---|---|---|---|
| Functions / API routes | Full | Full | `detect_start_cmd`/`adapter_manifest` emit `["bun", ...]` when runtime resolves to Bun; `CellBackend` execs it identically to any other argv | `git::tests::adapter_manifest_*_bun_runtime`, `bun_next_ssr_example_fixture_resolves_end_to_end`, live `bun-basic-api`/`bun-typescript-api` fixtures manually booted+curled | Generic `detect_start_cmd` fallback for a plain script always worked; only OpenNext/vinext/SvelteKit adapters needed the explicit Bun branch | Deploys and serves exactly like a Node function; `process.versions.bun` is set (Node compat shims also set `process.versions.node` — check `.bun`, not the absence of `.node`) |
| Next.js (plain `next start`) | Full | Runs, no bytecode cache | Build command becomes `bunx --bun next build` (adds `--bun` so Next's own internally-spawned Node-shebang child processes also run under Bun, per Vercel's documented requirement) | Build-command wrapping covered by `git.rs`'s existing build-cmd tests + manual verification of the `--bun` flag's real semantics via `bun run --help`/`bunx --help` | `next start` is a CLI wrapper, not a static module graph — `warmup_bun_bytecode` explicitly detects this shape and skips with a logged reason; the app still runs correctly | Works; no bytecode-cache cold-start speedup for this specific combination (documented, not silent) |
| Next.js via OpenNext/vinext adapter | Full (Fluid compute) | Full (Fluid compute) | `adapter_manifest` emits `["bun", <bundled-server-file>]`; both adapters already produce a single bundled server file, which IS a valid bytecode-cache target | `adapter_manifest_opennext_bun_runtime`, `adapter_manifest_vinext_bun_runtime_no_prebuilt_server`, `bun_next_ssr_example_fixture_resolves_end_to_end` (real fixture, manually booted under `bun run`) | None found — this is the recommended path for Bun+Next.js bytecode caching | Identical to Node except the interpreter; bytecode cache DOES apply here |
| SSR | Full | Full (via adapter) / Full-no-cache (plain `next start`) | Same as above — SSR execution is just "the function runs and renders", runtime-agnostic at the gateway/manifest layer | Same as Functions/Next.js rows | Same as Next.js rows | Same as Next.js rows |
| ISR | Full | Full | `RuntimeCache`/`origin_function` CDN-fallthrough logic never inspects runtime — pure HTTP+manifest mechanism | Not runtime-specific; existing ISR tests (`runtime_cache` module) apply unchanged since the function serving a revalidation is just an argv | None | Identical to Node |
| Workflows | Single-attempt HTTP dispatch, no retry | **Identical** — same code path | `WorkflowEngine`/`wf_invoker` dispatch via a plain outbound HTTP request to the deployment's own gateway (`Host: {deployment}.localhost`); it never inspects what runtime serves that request | Existing workflow tests unchanged (no runtime branching to test) | The native engine itself has no retry/idempotency-key handling — but this is **identical for Node and Bun today**, so it is not a Bun-parity gap. Real Vercel-WDK durable execution runs inside the user's own process via `@open-workflow/world-redis`, a library call that works identically under Bun's Node-compat `fetch`/Redis client | No difference from Node |
| Fluid compute / concurrent requests | Full | Full | `recommended_safe_concurrency` treats `"bun"` identically to `"node"`/`"js"` (full configured concurrency — Bun's event loop has the same I/O-friendly single-threaded characteristic); `FunctionPool`/`decide_lease`/AIMD never branch on runtime at all | `fluid-compute::tests::runtime_concurrency_policy` covers the `"bun"` arm | None | Identical scaling/reuse/backpressure behavior to Node |
| Regions | Full | Full | Placement scheduler (`schedule.rs`) only checks `backend=="firecracker"`/health/memory — never language | No new tests needed; scheduler is provably runtime-blind (confirmed by code inspection, no `f.runtime` read anywhere in `schedule.rs` except the `is_container` container check) | None | Identical region pinning/failover to Node |
| Streaming | Full | Full | Response streaming happens at the gateway/tunnel layer (byte passthrough); a Bun `Bun.serve` handler streams via the same Web `Response`/`ReadableStream` API Node's `undici`/fetch layer also exposes | Manually verified: `bun-basic-api`'s handlers use standard `Response` objects | None | Identical |
| waitUntil / background continuation | Supported via `x-fluid-wait-until-ms` response header convention | **Proven identical** | `fluid-gateway/src/lib.rs` parses the header off whatever process answered and holds the lease open in the background — no runtime-specific code exists | `fluid-gateway/tests/gateway.rs::wait_until_lease_held_open_after_response_for_a_bun_function` — full real stack (gateway -> Fluid pool -> mock instance -> tunnel -> genuine `bun` process running `examples/bun-basic-api`'s `/api/bg`), asserts `inflight >= 1` immediately after the response and `inflight == 0` once the window elapses | None found | Identical to Node |
| Logs | Full | Full | Build/runtime logs are captured by the platform's own log-forwarding (stdout/stderr capture), not a runtime-specific hook | Manually verified: fixture servers' `console.log` startup lines were visible when run directly | None | Identical |
| Metrics (incl. `node:http`/`https` request metrics) | Full (via gateway-level instrumentation) | **Full — stronger than Vercel's own Bun beta** | All request/latency/cold-start/status metrics (`Event`, `MetricsStore`, `FunctionStats`) are captured at the platform's own reverse-proxy/tunnel boundary, OUTSIDE the user's process — this sidesteps the exact gap Vercel's Bun beta has (their instrumentation hooks `node:http` internals directly, which don't exist under Bun) | Not newly tested (pre-existing black-box instrumentation, proven runtime-agnostic by inspection — `proxy_function` never inspects what's inside the box) | None found | Full metrics parity for Bun functions, unlike Vercel's documented Bun-beta gap |
| Bytecode cache | Full (`NODE_COMPILE_CACHE`, build-time V8 warm-up) | Full, structurally different mechanism | `warmup_bun_bytecode`: `bun build --bytecode --sourcemap=external --outdir=... <entry>` at build time, rewrites `start_cmd` to the bundled+cached output; `bun run <file>` auto-loads the `.jsc` sidecar with **zero runtime env var** | `git::tests::warmup_bun_bytecode_real_bundle_and_bytecode_cache` (real `bun build`, asserts `.jsc`+`.map` exist AND the bundled server actually boots), `bun_bundle_entry_*` unit tests | Only applies to a real bundleable entry FILE (`bun <file>`/`bun run <file>`) — a CLI wrapper (`bunx --bun next start`) is explicitly detected and skipped with a logged reason, not silently faked | First deploy bundles+caches; redeploys reuse the cache; corrupted/missing cache falls back to running the original entry uncached, never fails the deploy |
| Source maps | N/A (Node doesn't strip TS by default; Vercel's own Bun beta does NOT emit automatic source maps) | **Full — stronger than Vercel's Bun beta** | Un-bundled Bun execution (`bun run server.ts`) gets accurate `.ts` source stack traces natively, free, with zero platform code (verified live: `bun-typescript-api`'s `/api/type-error` route). The BUNDLED bytecode-cache path explicitly passes `--sourcemap=external` so bundled-artifact stack traces still resolve to real source | Manual live verification (real `TypeError` thrown from `.ts`, stack trace pointed at the exact source line); `.map` file existence asserted in the bytecode-cache test | None found | Bun users get real source-accurate errors in both the un-bundled and bundled paths — Vercel's own beta lacks this entirely |
| Env vars | Full | Full | Env injection (`FunctionConfig.env`, project env vars) is copied into `FunctionLaunch.env` uniformly; `CellBackend`/guest agent `envs()` the same map regardless of runtime | Pre-existing env-injection tests unchanged (no runtime branching) | None | Identical |
| Secrets | Full | Full | Same injection path as env vars; never touches source, cache, or bytecode-cache artifacts | None new; the bytecode-cache bundling step only bundles the ENTRY module's own imports, never env/secret values | None | Identical; secrets are never baked into the `.jsc`/bundle since they're runtime env, not compile-time constants |
| Monorepos | Full (pnpm gets `--filter` install-scoping) | **Proven working**, no `--filter` scoping | Monorepo/workspace detection (`is_monorepo`, `foreign_subdir`, install-dir resolution) never inspects runtime — orthogonal to the runtime axis. Bun's own `bun install` (no filter) installs the WHOLE workspace, which is correct but does more work than pnpm's path-scoped install | `git::tests::bun_monorepo_example_fixture_detected_as_monorepo_with_bun_pm` against a real `examples/bun-monorepo` fixture; manually verified live: a real `bun install` at the root correctly symlinked a `workspace:*` dependency, and the dependent package's server (run via the platform's exact `bun run --bun start`) correctly imported and called into it | **Minor, non-blocking gap found**: unlike pnpm, bun installs are not scoped to just the target package + its deps in a monorepo (bun's `--filter` takes a package NAME, not pnpm's path-selector syntax, so wiring it in would need an extra package.json read) — correctness is unaffected, only install-time efficiency on very large monorepos | Works correctly; a monorepo install may do somewhat more work than strictly necessary compared to the pnpm path |
| Package-manager detection | Full (bun/pnpm/yarn/npm via lockfiles) | Full, now with Corepack precedence | `detect_package_manager`: `package.json#packageManager` > `bun.lock` > `bun.lockb` > `pnpm-lock.yaml` > `yarn.lock` > `package-lock.json` > default | 7 new unit tests in `framework.rs` (Corepack precedence, conflicting lockfiles, malformed field, never-deletes-lockfiles) + `bun_fixtures.rs` integration tests against real example repos | None found | Build logs show the detected manager + its source (`Corepack`/`BunLock`/etc.) and any conflict warning |
| Custom install/build command | Full | Full | User-supplied `installCommand`/`buildCommand` overrides pass through unchanged; the only Bun-specific rewrite is adding `--bun` when the runtime is explicitly Bun and the command isn't already a raw package-manager invocation | Existing override tests (`build_override_wins`) unaffected; `--bun` wrapping covered by manual flag verification | None | Explicit overrides are honored exactly as configured; only the implicit "wrap a bare binary invocation" path adds `--bun` |
| Next.js | See above rows | See above rows | — | — | — | — |
| Express | Full | **Proven, and caught a real bug** | Served via the generic `detect_start_cmd` `scripts.start` path | Real `npm`-published `express@^4.19.2` installed via `bun install`; live-verified via `examples/bun-express-api` (`"start": "node server.js"`) | **Found and fixed a real bug**: `detect_start_cmd` used to emit `["bun","run","start"]`, which re-executes the script's own text ("node server.js") as a shell command — Bun's script-runner spawned REAL Node (`process.versions.bun` was `null`), silently defeating the Bun runtime choice. Fixed by adding `--bun` (`["bun","run","--bun","start"]`), which forces Bun to substitute itself for the node-shebang child the script invokes. This was the MOST COMMON real-world shape (any project with a plain `"start": "node ..."` script), so this bug would have silently affected most real Bun deployments | Per-request error isolation confirmed (an unhandled throw in one request doesn't take down others); `process.versions.bun` now correctly reports the real version |
| Hono | Full | **Proven, clean pass** | Same generic path; Hono is Bun-first (idiomatic `export default {port, fetch}` serving convention) | Real `hono@^4.6.0` installed via `bun install`; live-verified via `examples/bun-hono-api` under the exact `bun run --bun start` invocation | None found | Works correctly out of the box; no fix needed |
| Nitro | Full | Full | Nitro is vinext's underlying server engine — covered by the vinext adapter rows above | See vinext rows | None found beyond what vinext already covers | Same as vinext |
| Middleware (Next.js) | Runs inside the framework's own server process | **Proven identical to Node** | The platform has no code that inspects or executes Next.js middleware directly | Real Next.js 14 app with a real `middleware.js`, real `next`/`react`/`react-dom` deps, built via the platform's EXACT `bunx --bun next build` and served via the EXACT `bun run --bun start` invocation (`examples/bun-next-middleware`; `git::tests::bun_next_middleware_example_fixture_resolves_bun_start_and_framework`); curled live, `x-middleware-ran: true` present on every response | None found — and a clarifying discovery: Next.js runs middleware inside its OWN sandboxed Edge Runtime, which does not expose a `Bun` global even under a genuinely-Bun host process (confirmed: a `typeof Bun` probe inside the middleware itself reported `"not-bun"` even while the outer process was real Bun). This means middleware behavior is identical under Node and Bun BY CONSTRUCTION, not by platform-level effort | Fully functional; indistinguishable from running under Node |

## Verification commands

```bash
# Core Runtime enum + FunctionLaunch wiring
cargo test -p hive-core

# Package-manager detection (Corepack precedence, conflicts, real fixtures)
cargo test -p fluid-build

# Bun bytecode cache (real `bun build --bytecode`, asserts .jsc+.map, boots the bundle)
cargo test -p hive-cloud --features zkauth git::tests::warmup_bun_bytecode_real_bundle_and_bytecode_cache

# Runtime resolution, adapter manifests (Node+Bun), fixture-backed end-to-end test
cargo test -p hive-cloud --features zkauth git::

# Fluid compute concurrency policy (includes the "bun" arm)
cargo test -p fluid-compute

# Cold-start concurrency scaling + reflink rootfs copy (Task 1 perf)
cargo test -p fluid-compute max_concurrent_cold_starts
cargo test -p hive-backend reflink

# waitUntil through the full real stack (gateway -> Fluid -> tunnel -> genuine bun process)
cargo test -p fluid-gateway --test gateway wait_until_lease_held

# Express/Hono/monorepo/middleware fixture-backed regression + positive-finding tests
cargo test -p hive-cloud --features zkauth bun_express
cargo test -p hive-cloud --features zkauth bun_hono
cargo test -p hive-cloud --features zkauth bun_monorepo
cargo test -p hive-cloud --features zkauth bun_next_middleware
cargo test -p hive-cloud --features zkauth detect_start_cmd_regression

# Whole workspace, zero warnings-as-errors expected
cargo check --workspace --features hive-cloud/zkauth
```

Manual live verification performed during this work (not automated, but real):
`bun run` against `examples/bun-basic-api/server.js` (HTTP round trip,
per-request error isolation, `process.versions.bun` presence),
`examples/bun-typescript-api/server.ts` (native TS execution, real
source-accurate stack trace),
`examples/bun-next-ssr/.open-next/server-functions/default/index.mjs` (proves
the exact adapter-emitted start command boots under Bun),
`examples/bun-express-api` and `examples/bun-hono-api` (real `npm`-published
packages, real `bun install`, real HTTP round trips — the Express fixture is
what caught the `--bun` flag bug),
`examples/bun-next-middleware` (a real Next.js 14 app built and served via the
platform's EXACT generated commands, `bunx --bun next build` +
`bun run --bun start`, proving middleware executes correctly), and
`examples/bun-monorepo` (a real Bun workspace with a `workspace:*` dependency,
proving `bun install` at the root links and runs correctly).

## How to enable Bun

Either of:

```json
// vercel.json — platform-native selector
{ "runtime": "bun" }
```

```json
// vercel.json — Vercel's own Bun-beta selector (also works)
{ "bunVersion": "1.x" }
```

A `bun.lock`/`bun.lockb` in the repo alone only selects `bun install` as the
package manager — it does **not** enable the Bun runtime by itself.

## Non-negotiables honored

No placeholder support: every row above that claims "Full" was either
exercised by a real test against a real `bun` binary, or is backed by code
inspection proving the relevant subsystem never branches on runtime at all.

A follow-up verification pass installed real Express/Hono/Next.js packages,
built a real monorepo, and exercised `waitUntil` through the full live stack —
everything previously marked "not independently verified" now has a real
fixture and a real test. That pass found and fixed one genuine bug: `bun run
start` on a `"start": "node ..."` script silently ran real Node, not Bun
(fixed by adding `--bun`); every other previously-flagged item (Hono,
middleware, monorepo, waitUntil) turned out to already work correctly.
