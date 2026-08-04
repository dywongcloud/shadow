#!/usr/bin/env node
// Syncs the built hive-browser wasm bundle (crates/hive-browser/www/{pkg,identity.js})
// into ui/public/browser-node/ so the dashboard's "Run a node" SharedWorker can load
// it as a same-origin static asset. The wasm build is a real generated artifact
// (crates/hive-browser/build.sh) -- this script keeps ui/public in sync with it
// rather than letting a manual one-time copy silently go stale. A missing source
// (hive-browser not yet built) is a WARN, never a build failure: the dashboard
// still builds and serves everything else; "Run a node" degrades to its own
// explicit unsupported-browser/missing-bundle state at runtime.
import { existsSync, mkdirSync, readdirSync, copyFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const uiDir = dirname(dirname(fileURLToPath(import.meta.url)));
const srcRoot = join(uiDir, "..", "crates", "hive-browser", "www");
const srcPkg = join(srcRoot, "pkg");
const destRoot = join(uiDir, "public", "browser-node");
const destPkg = join(destRoot, "pkg");

if (!existsSync(srcPkg)) {
  console.warn(`[sync-browser-node] ${srcPkg} not built yet — skipping (run crates/hive-browser/build.sh first). "Run a node" will report an unsupported/missing-bundle state until then.`);
  process.exit(0);
}

mkdirSync(destPkg, { recursive: true });
for (const f of readdirSync(srcPkg)) {
  if (f.endsWith(".wasm") || f.endsWith(".js") || f.endsWith(".d.ts")) {
    copyFileSync(join(srcPkg, f), join(destPkg, f));
  }
}
// Worker-loadable ES modules the SharedWorker imports directly (identity for
// both, plus the browser-worker-quickjs-runtime lane: the canonical policy
// digest module and the worker-native QuickJS runtime). These are hand-written
// sources in www/, not build artifacts — pkg/ alone is not enough.
for (const f of ["identity.js", "artifact-policy.js", "worker-function-runtime.js"]) {
  const src = join(srcRoot, f);
  if (existsSync(src)) {
    copyFileSync(src, join(destRoot, f));
  } else if (f !== "identity.js") {
    console.warn(`[sync-browser-node] ${src} missing — the worker QuickJS lane will fail to load until it exists.`);
  }
}
console.log(`[sync-browser-node] synced ${srcPkg} -> ${destPkg}`);
