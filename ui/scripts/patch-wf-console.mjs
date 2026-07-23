// scripts/patch-wf-console.mjs
//
// Idempotent build-time patches over the compiled @workflow/web bundles in
// node_modules, so the LITERAL upstream WDK console serves at /wfc and is
// embedded (iframe) inside the dashboard's own /workflows page — platform
// navbar on top, zero upstream chrome — reading hive data through the World
// proxy (lib/wf-console/hive-world.mjs). The upstream package ships compiled
// Vite bundles only — there is no source to fork; every customization is a
// targeted, sentinel-guarded string replacement (the agentos
// patch-workflow-web.mjs discipline).
//
// IMPORTANT: anchors target the PRISTINE upstream bundles. If patches report
// `no-anchor`, reinstall the package fresh first:
//   (cd ui && npm i --no-save @workflow/web@<locked version>)
// then re-run. Re-runs over an already-patched tree are no-ops (sentinels).
//
// Patches:
//   1. basename        "/" -> "/wfc" in the React Router builds. The console
//                      is served under /wfc (next.config rewrites
//                      /wfc/:path* -> /wf-console/wfc/:path*), while the
//                      user-facing /workflows page is a normal dashboard page
//                      (platform navbar) embedding /wfc in a seamless iframe.
//   2. rpc guard       the client hardcodes fetch("/api/rpc"). Replace with a
//                      guarded fetch to /wfc/api/rpc that (a) carries the
//                      hive team/project scope the parent page stores in
//                      window.name ("wfc|<team>|<project>"), (b) verifies the
//                      response is actually CBOR before decoding — an auth
//                      redirect or error page previously fed HTML to cbor-x
//                      and surfaced as "Unknown token 28", (c) re-mints the
//                      hive_jwt session ONCE via POST /api/token and retries
//                      on a non-ok/non-CBOR response, so an idle tab recovers
//                      instead of erroring.
//   3. stream URL      `/api/stream/...` -> `/wfc/api/stream/...`.
//   4. isLocalBackend  force TRUE (and force backendId/@displayName) so the
//                      Workflows/Graph tabs render — the gate would otherwise
//                      hide them because WORKFLOW_TARGET_WORLD points at a
//                      custom module path, not "@workflow/world-local".
//   5. manifest        fetchWorkflowsManifest reads from disk upstream;
//                      delegate to world.hiveFetchWorkflowsManifest() (hive's
//                      /v1/workflows defs reshaped) when the World offers it.
//   6. run ops         cancel/recreate/reenqueue/wakeUp are composed from
//                      World primitives upstream (events.create + queue +
//                      full event walks); delegate each to world.hiveOps.*
//                      which maps 1:1 onto hive's dedicated run-op endpoints.
//   7. hide header     the upstream console's own sticky navbar (Workflow
//                      logo, "Local Dev: …", Health Check, theme toggle,
//                      Docs) is display:none'd in BOTH client and SSR bundles
//                      (hydration-consistent) — the shadw platform navbar is
//                      the only chrome. The in-page Runs/Hooks/Workflows
//                      segmented control is page content and survives.
//   8. theme sync      the console's ThemeProvider keeps its own storage key
//                      ("workflow-theme") with NO forcedTheme — the platform
//                      page (components/wf-console-frame.tsx) writes that key
//                      from the dashboard's next-themes state before mount and
//                      on every toggle, so the console follows the platform's
//                      light/dark live. defaultTheme "dark" = fallback for a
//                      direct /wfc visit only.
//
// Run before every `next build` (see package.json "build"). Re-runs are
// no-ops. If an anchor is missing after an upstream version bump, the run
// reports `no-anchor` for that patch — re-locate the needle in the new
// bundle and update it here.

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const UI_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const WEB_PKG = path.join(UI_ROOT, "node_modules", "@workflow", "web");
const BUILD = path.join(WEB_PKG, "build");

const BASENAME = "/wfc";

function patchBasename(file, s) {
  const needle = `const basename = "/";\nconst future = {`;
  const replace = `const basename = "${BASENAME}";\nconst future = {`;
  if (s.includes(replace)) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replace(needle, replace), status: "patched" };
}

// The single data-plane choke point: every console read (fetchRuns/fetchRun/
// fetchEvents/… and the manifest + run-op server actions) rides this one
// rpc() helper. Guarding here covers the entire console.
function patchRpcGuard(file, s) {
  const needle =
    `async function rpc(method, params) {\n` +
    `  var _a3;\n` +
    `  const res = await fetch("/api/rpc", {\n` +
    `    method: "POST",\n` +
    `    headers: {\n` +
    `      "Content-Type": "application/cbor",\n` +
    `      Accept: "application/cbor"\n` +
    `    },\n` +
    `    body: new Uint8Array(encode({ method, params: params ?? {} }))\n` +
    `  });\n` +
    `  if (!res.ok) {\n`;
  const replace =
    `async function rpc(method, params) {\n` +
    `  var _a3;\n` +
    `  /* __hive_rpc_guard */\n` +
    `  const __hiveName = () => {\n` +
    `    try {\n` +
    `      if (typeof window === "undefined") return null;\n` +
    `      return /^wfc\\|([^|]*)\\|(.*)$/.exec(window.name || "");\n` +
    `    } catch (_e) { return null; }\n` +
    `  };\n` +
    `  const __hiveScope = () => {\n` +
    `    const m = __hiveName();\n` +
    `    if (!m) return "";\n` +
    `    const qs = new URLSearchParams();\n` +
    `    if (m[1]) qs.set("hiveTeam", m[1]);\n` +
    `    if (m[2]) qs.set("hiveProject", m[2]);\n` +
    `    const str = qs.toString();\n` +
    `    return str ? "?" + str : "";\n` +
    `  };\n` +
    `  const __hiveFetch = () => fetch("${BASENAME}/api/rpc" + __hiveScope(), {\n` +
    `    method: "POST",\n` +
    `    headers: {\n` +
    `      "Content-Type": "application/cbor",\n` +
    `      Accept: "application/cbor"\n` +
    `    },\n` +
    `    body: new Uint8Array(encode({ method, params: params ?? {} }))\n` +
    `  });\n` +
    `  let res = await __hiveFetch();\n` +
    `  const __hiveIsCbor = () => (res.headers.get("content-type") || "").includes("cbor");\n` +
    `  if ((!res.ok || !__hiveIsCbor()) && typeof window !== "undefined") {\n` +
    `    try {\n` +
    `      const m = __hiveName();\n` +
    `      await fetch("/api/token", {\n` +
    `        method: "POST",\n` +
    `        headers: { "content-type": "application/json" },\n` +
    `        body: JSON.stringify(m && m[1] ? { team: m[1] } : {})\n` +
    `      });\n` +
    `    } catch (_e) {}\n` +
    `    res = await __hiveFetch();\n` +
    `  }\n` +
    `  if (res.ok && !__hiveIsCbor()) {\n` +
    `    throw new Error(\`RPC call \${method} failed: unexpected non-CBOR response (\${res.status}) — session may have expired, reload the page\`);\n` +
    `  }\n` +
    `  if (!res.ok) {\n` +
    `    if (!__hiveIsCbor()) {\n` +
    `      throw new Error(\`RPC call \${method} failed: \${res.status} \${res.statusText}\`);\n` +
    `    }\n`;
  if (s.includes("__hive_rpc_guard")) return { s, status: "skip-already" };
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

// The upstream console's own sticky navbar (Workflow logo + ConnectionStatus
// "Local Dev: …" + HealthCheckButton + ThemePicker + DocsLink). display:none
// keeps client and SSR renders identical (no hydration mismatch) while the
// shadw platform navbar (in the parent page) is the only visible chrome.
function patchHideHeader(file, s) {
  const needle = `className: "sticky top-0 z-50 bg-background border-b px-6 py-4",`;
  const replace =
    `className: "sticky top-0 z-50 bg-background border-b px-6 py-4",\n` +
    `        style: { display: "none" }, /* __hive_hide_header */`;
  if (s.includes("__hive_hide_header")) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replaceAll(needle, replace), status: "patched" };
}

// Theme SYNC (not force): the platform's WfConsoleFrame drives the console's
// next-themes storage key ("workflow-theme") from the dashboard's own
// next-themes state — pre-written before the iframe mounts (correct FIRST
// paint via the console's SSR theme bootstrap) and live-updated on every
// platform toggle (same-origin storage events reach the iframe's cross-tab
// listener; the frame also reconciles the root class directly). So the
// provider must NOT carry forcedTheme (an earlier patch generation forced
// dark, which broke light-mode users — the reported bug). defaultTheme
// "dark" stays as the brand fallback for a direct /wfc visit with no stored
// key.
function patchThemeSync(file, s) {
  const needle = `attribute: "class",\n    defaultTheme: "system"`;
  const replace = `attribute: "class",\n    defaultTheme: "dark" /* __hive_theme_sync */`;
  if (s.includes("__hive_theme_sync")) return { s, status: "skip-already" };
  if (!s.includes(needle)) return { s, status: "no-anchor" };
  return { s: s.replaceAll(needle, replace), status: "patched" };
}

const PATCHES = [
  ["basename", patchBasename],
  ["rpc-guard", patchRpcGuard],
  ["stream-url", patchStreamUrl],
  ["local-gate", patchIsLocalGate],
  ["backend-id", patchBackendId],
  ["display-name", patchDisplayName],
  ["manifest", patchManifestDelegation],
  ["run-ops", patchRunOps],
  ["hide-header", patchHideHeader],
  ["theme-sync", patchThemeSync],
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

// Patching edits content-hashed, `immutable, max-age=1y`-cached client chunks
// IN PLACE — serving changed bytes under an unchanged filename. A browser that
// re-fetches ONE such chunk while holding immutable-cached copies of its
// siblings ends up with a mixed module graph, which dies at import time
// ("module './mermaid-…js' does not provide an export named 'B'") and kills
// the console's hydration silently and PERMANENTLY (the poisoned entries never
// revalidate). Fix at the root: any client chunk this patch run modified gets
// a NEW name carrying a digest of its patched content (`x.h<8hex>.js`), and
// every reference across the whole build (sibling chunk imports, the server
// build's SSR asset manifest/preloads) is rewritten. Unmodified chunks keep
// their upstream names — their bytes never change, so cached copies stay
// valid. Idempotent: a re-run strips the previous marker, re-digests, and
// no-ops when content is unchanged.
function rehashPatchedClientAssets(dirtied) {
  const clientAssets = path.join(BUILD, "client", "assets");
  if (!fs.existsSync(clientAssets)) return;
  const marker = /\.h[0-9a-f]{8}(?=\.(js|css)$)/;
  const renames = new Map(); // old basename -> new basename
  for (const f of fs.readdirSync(clientAssets)) {
    const full = path.join(clientAssets, f);
    const hadMarker = marker.test(f);
    if (!dirtied.has(full) && !hadMarker) continue;
    const content = fs.readFileSync(full);
    const digest = crypto.createHash("sha256").update(content).digest("hex").slice(0, 8);
    const stripped = f.replace(marker, "");
    const ext = path.extname(stripped);
    const next = `${stripped.slice(0, -ext.length)}.h${digest}${ext}`;
    if (next === f) continue;
    fs.renameSync(full, path.join(clientAssets, next));
    renames.set(f, next);
    console.log(`[patch-wf-console] rehash ${f} -> ${next}`);
  }
  if (renames.size === 0) return;
  // Rewrite every reference across the build (basenames are high-entropy and
  // globally unique, so a blind replaceAll is safe).
  const targets = [];
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else if (/\.(js|mjs|css|html|json|map)$/.test(e.name)) targets.push(p);
    }
  };
  walk(BUILD);
  for (const t of targets) {
    let s = fs.readFileSync(t, "utf8");
    let dirty = false;
    for (const [from, to] of renames) {
      if (s.includes(from)) {
        s = s.replaceAll(from, to);
        dirty = true;
      }
    }
    if (dirty) fs.writeFileSync(t, s);
  }
}

function main() {
  if (!fs.existsSync(BUILD)) {
    console.log("[patch-wf-console] @workflow/web not installed — skipping");
    return;
  }
  const files = collectBundleFiles();
  const totals = {};
  const dirtied = new Set();
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
    if (dirty) {
      fs.writeFileSync(f, s);
      dirtied.add(f);
    }
  }
  rehashPatchedClientAssets(dirtied);
  const summary = PATCHES.map(([n]) => `${n}:${totals[n] || 0}`).join(" ");
  console.log(`[patch-wf-console] done — ${summary} (files:${files.length})`);
  // The basename + rpc-guard + header patches are load-bearing: fail loudly
  // when the server build has none of them applied and none pre-applied
  // (e.g. after an upstream version bump moved the anchors, or the tree is
  // still carrying an OLD patch generation — reinstall fresh, then re-run).
  const serverBuild = files.find((f) => path.basename(f).startsWith("server-build-"));
  if (serverBuild) {
    const s = fs.readFileSync(serverBuild, "utf8");
    const ok =
      s.includes(`const basename = "${BASENAME}";`) &&
      s.includes("__hive_rpc_guard") &&
      s.includes("__hive_hide_header") &&
      s.includes("__hive_theme_sync") &&
      s.includes("__hive_backend_id") &&
      s.includes("__hive_manifest_delegate") &&
      s.includes("__hive_op_cancel");
    if (!ok) {
      console.error(
        "[patch-wf-console] WARNING: server build is missing load-bearing patches — reinstall @workflow/web fresh and re-run, or re-locate anchors against the installed version"
      );
      process.exitCode = 1;
    }
  }
}

main();
