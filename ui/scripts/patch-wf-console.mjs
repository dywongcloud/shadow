// scripts/patch-wf-console.mjs
//
// Idempotent build-time patches over the compiled @workflow/web bundles in
// node_modules, so the LITERAL upstream WDK console mounts at /workflows
// inside hive's dashboard and reads hive data through the World proxy
// (lib/wf-console/hive-world.mjs). The upstream package ships compiled
// Vite bundles only — there is no source to fork; every customization is a
// targeted, sentinel-guarded string replacement (the agentos
// patch-workflow-web.mjs discipline).
//
// Patches:
//   1. basename        "/" -> "/workflows" in the React Router server build.
//                      The SSR handler matches routes under /workflows and
//                      streams the basename to the client, so hydration,
//                      in-app navigation, /__manifest and .data loader URLs
//                      all agree — no iframe, no URL mismatch.
//   2. rpc/stream URLs the client hardcodes fetch("/api/rpc") and
//                      `/api/stream/...` at the root; re-point both under
//                      /workflows so they ride the same rewrite (and hive's
//                      own /api/* namespace stays untouched).
//   3. isLocalBackend  force TRUE (and force backendId/@displayName) so the
//                      Workflows/Graph tabs render — the gate would otherwise
//                      hide them because WORKFLOW_TARGET_WORLD points at a
//                      custom module path, not "@workflow/world-local".
//   4. manifest        fetchWorkflowsManifest reads from disk upstream;
//                      delegate to world.hiveFetchWorkflowsManifest() (hive's
//                      /v1/workflows defs reshaped) when the World offers it.
//   5. run ops         cancel/recreate/reenqueue/wakeUp are composed from
//                      World primitives upstream (events.create + queue +
//                      full event walks); delegate each to world.hiveOps.*
//                      which maps 1:1 onto hive's dedicated run-op endpoints.
//
// Run before every `next build` (see package.json "build"). Re-runs are
// no-ops. If an anchor is missing after an upstream version bump, the run
// reports `no-anchor` for that patch — re-locate the needle in the new
// bundle and update it here.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const UI_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const WEB_PKG = path.join(UI_ROOT, "node_modules", "@workflow", "web");
const BUILD = path.join(WEB_PKG, "build");

const BASENAME = "/workflows";

function patchBasename(file, s) {
  const needle = `const basename = "/";\nconst future = {`;
  const replace = `const basename = "${BASENAME}";\nconst future = {`;
  if (s.includes(replace)) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replace(needle, replace), status: "patched" };
}

function patchRpcUrl(file, s) {
  const needle = `fetch("/api/rpc", {`;
  const replace = `fetch("${BASENAME}/api/rpc", {`;
  if (s.includes(replace)) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replaceAll(needle, replace), status: "patched" };
}

function patchStreamUrl(file, s) {
  const needle = "`/api/stream/${encodeURIComponent(streamId)}?${params.toString()}`";
  const replace = "`" + BASENAME + "/api/stream/${encodeURIComponent(streamId)}?${params.toString()}`";
  if (s.includes(replace)) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replaceAll(needle, replace), status: "patched" };
}

function patchIsLocalGate(file, s) {
  const needle =
    `serverConfig.backendId === "local" || serverConfig.backendId === "@workflow/world-local"`;
  const replace = `true /* __hive_local_gate */`;
  if (s.includes(replace)) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replaceAll(needle, replace), status: "patched" };
}

function patchBackendId(file, s) {
  const needle = `function getEffectiveBackendId() {`;
  const replace =
    `function getEffectiveBackendId() {\n  return "@workflow/world-local"; /* __hive_backend_id */`;
  if (s.includes("__hive_backend_id")) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replace(needle, replace), status: "patched" };
}

function patchDisplayName(file, s) {
  const needle = `function getBackendDisplayName(targetWorld) {`;
  const replace =
    `function getBackendDisplayName(targetWorld) {\n  return "hive"; /* __hive_display_name */`;
  if (s.includes("__hive_display_name")) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replace(needle, replace), status: "patched" };
}

function patchManifestDelegation(file, s) {
  const needle = `async function fetchWorkflowsManifest(_worldEnv) {\n  await ensureLocalWorldDataDirEnv();`;
  const replace =
    `async function fetchWorkflowsManifest(_worldEnv) {\n` +
    `  /* __hive_manifest_delegate */\n` +
    `  try {\n` +
    `    const __hiveWorld = await getWorldFromEnv(_worldEnv ?? {});\n` +
    `    if (__hiveWorld && typeof __hiveWorld.hiveFetchWorkflowsManifest === "function") {\n` +
    `      const __m = await __hiveWorld.hiveFetchWorkflowsManifest();\n` +
    `      if (__m) return createResponse(__m);\n` +
    `    }\n` +
    `  } catch (_e) {}\n` +
    `  await ensureLocalWorldDataDirEnv();`;
  if (s.includes("__hive_manifest_delegate")) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replace(needle, replace), status: "patched" };
}

// The four run-op server actions. Each gets an early delegation to
// world.hiveOps.<op> right after its `const world = await getWorldFromEnv…`
// line, keeping the upstream primitive-composed implementation as fallback.
const RUN_OP_PATCHES = [
  {
    sentinel: "__hive_op_cancel",
    needle:
      `    const world = await getWorldFromEnv(worldEnv);\n` +
      `    await cancelRun$2(world, runId);`,
    replace:
      `    const world = await getWorldFromEnv(worldEnv);\n` +
      `    if (world.hiveOps && world.hiveOps.cancelRun) { await world.hiveOps.cancelRun(runId); return createResponse(void 0); } /* __hive_op_cancel */\n` +
      `    await cancelRun$2(world, runId);`,
  },
  {
    sentinel: "__hive_op_recreate",
    needle:
      `    const world = await getWorldFromEnv({ ...worldEnv });\n` +
      `    const newRunId = await recreateRunFromExisting(`,
    replace:
      `    const world = await getWorldFromEnv({ ...worldEnv });\n` +
      `    if (world.hiveOps && world.hiveOps.recreateRun) { return createResponse(await world.hiveOps.recreateRun(runId, deploymentId)); } /* __hive_op_recreate */\n` +
      `    const newRunId = await recreateRunFromExisting(`,
  },
  {
    sentinel: "__hive_op_reenqueue",
    needle:
      `    const world = await getWorldFromEnv({ ...worldEnv });\n` +
      `    await reenqueueRun$2(world, runId);`,
    replace:
      `    const world = await getWorldFromEnv({ ...worldEnv });\n` +
      `    if (world.hiveOps && world.hiveOps.reenqueueRun) { await world.hiveOps.reenqueueRun(runId); return createResponse(void 0); } /* __hive_op_reenqueue */\n` +
      `    await reenqueueRun$2(world, runId);`,
  },
  {
    sentinel: "__hive_op_wakeup",
    needle:
      `    const world = await getWorldFromEnv({ ...worldEnv });\n` +
      `    const result = await wakeUpRun$2(world, runId, options);`,
    replace:
      `    const world = await getWorldFromEnv({ ...worldEnv });\n` +
      `    if (world.hiveOps && world.hiveOps.wakeUpRun) { return createResponse(await world.hiveOps.wakeUpRun(runId, options)); } /* __hive_op_wakeup */\n` +
      `    const result = await wakeUpRun$2(world, runId, options);`,
  },
];

function patchRunOps(file, s) {
  let changed = false;
  let missing = 0;
  for (const p of RUN_OP_PATCHES) {
    if (s.includes(p.sentinel)) continue;
    if (!s.includes(p.needle)) {
      missing++;
      continue;
    }
    s = s.replace(p.needle, p.replace);
    changed = true;
  }
  if (changed) return { s, status: "patched" };
  if (missing === RUN_OP_PATCHES.length) return { s, status: "no-anchor" };
  return { s, status: "skip-already" };
}

const PATCHES = [
  ["basename", patchBasename],
  ["rpc-url", patchRpcUrl],
  ["stream-url", patchStreamUrl],
  ["local-gate", patchIsLocalGate],
  ["backend-id", patchBackendId],
  ["display-name", patchDisplayName],
  ["manifest", patchManifestDelegation],
  ["run-ops", patchRunOps],
];

function collectBundleFiles() {
  const dirs = [path.join(BUILD, "client", "assets"), path.join(BUILD, "server", "assets")];
  return dirs.flatMap((d) =>
    fs.existsSync(d)
      ? fs
          .readdirSync(d)
          .filter((f) => f.endsWith(".js"))
          .map((f) => path.join(d, f))
      : []
  );
}

function main() {
  if (!fs.existsSync(BUILD)) {
    console.log("[patch-wf-console] @workflow/web not installed — skipping");
    return;
  }
  const files = collectBundleFiles();
  const totals = {};
  for (const f of files) {
    let s = fs.readFileSync(f, "utf8");
    let dirty = false;
    for (const [name, fn] of PATCHES) {
      const r = fn(f, s);
      if (r.status === "patched") {
        s = r.s;
        dirty = true;
        totals[name] = (totals[name] || 0) + 1;
        console.log(`[patch-wf-console] ${name} -> ${path.relative(WEB_PKG, f)}`);
      }
    }
    if (dirty) fs.writeFileSync(f, s);
  }
  const summary = PATCHES.map(([n]) => `${n}:${totals[n] || 0}`).join(" ");
  console.log(`[patch-wf-console] done — ${summary} (files:${files.length})`);
  // The basename + gate + rpc patches are load-bearing: warn loudly when a
  // fresh install has none of them applied and none pre-applied.
  const serverBuild = files.find((f) => path.basename(f).startsWith("server-build-"));
  if (serverBuild) {
    const s = fs.readFileSync(serverBuild, "utf8");
    const ok =
      s.includes(`const basename = "${BASENAME}";`) &&
      s.includes("__hive_backend_id") &&
      s.includes("__hive_manifest_delegate") &&
      s.includes("__hive_op_cancel");
    if (!ok) {
      console.error(
        "[patch-wf-console] WARNING: server build is missing load-bearing patches — check anchors against the installed @workflow/web version"
      );
      process.exitCode = 1;
    }
  }
}

main();
