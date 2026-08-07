#!/usr/bin/env node
// Syncs the built hive-browser wasm bundle (crates/hive-browser/www/{pkg,identity.js})
// into ui/public/browser-node/ so the dashboard's "Run a node" SharedWorker can load
// it as a same-origin static asset. The wasm build is a real generated artifact
// (crates/hive-browser/build.sh) -- this script keeps ui/public in sync with it
// rather than letting a manual one-time copy silently go stale. A missing source
// (hive-browser not yet built) is a WARN, never a build failure: the dashboard
// still builds and serves everything else; "Run a node" degrades to its own
// explicit unsupported-browser/missing-bundle state at runtime.
import { existsSync, mkdirSync, readdirSync, copyFileSync, readFileSync } from "node:fs";
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
// digest module and the worker-native QuickJS runtime, and the browser-to-
// browser WebRTC mesh lane). These are hand-written sources in www/, not build
// artifacts — pkg/ alone is not enough.
//
// peer-mesh*.js are load-bearing here: run-node-worker.js resolves them with a
// RUNTIME dynamic import against its own deployed origin, so omitting them from
// this list does not fail any build — it 404s in the browser at the moment the
// mesh lane starts, on the fleet only. That is exactly what happened: both files
// existed in crates/hive-browser/www/ and neither was ever copied to
// ui/public/browser-node/, so the deployed worker imported a path that was never
// published.
//
// node-runtime.js is a STATIC import of worker-function-runtime.js
// (browser-node-api-runtime): omitting it does not 404 one lane, it fails the
// SharedWorker's whole module graph on the fleet while every local check stays
// green — the same publishing gap as peer-mesh*.js, one step louder.
for (const f of [
  "identity.js",
  "artifact-policy.js",
  "node-runtime.js",
  "worker-function-runtime.js",
  "peer-mesh.js",
  "peer-mesh-agent.js",
]) {
  const src = join(srcRoot, f);
  if (existsSync(src)) {
    copyFileSync(src, join(destRoot, f));
  } else if (f !== "identity.js") {
    console.warn(`[sync-browser-node] ${src} missing — the worker QuickJS lane will fail to load until it exists.`);
  }
}

// The browser-replicated database lane (bn-run-node-db-sync-wiring): when the
// admission capability carries a `db` block, run-node-worker.js spawns the
// sqlite DedicatedWorker from these deployed assets. The set is the exact
// runtime import graph of sqlite-worker.js + sync-client.js (crsqlite-sync.mjs
// loads crsqlite-sync.wasm relative to its own import.meta.url, so both must
// land in the same directory) — every file the worker can reach, nothing
// more. crsqlite-sync.{mjs,wasm} are build artifacts of
// crates/hive-browser/www/sqlite/build-sqlite.sh; the rest are hand-written
// sources. A missing source is a WARN, never a build failure — the same
// discipline as pkg/ above (the db lane reports its own honest error at
// runtime; "Run a node" itself still works).
const SQLITE_SET = [
  "sqlite/sqlite-worker.js",
  "sqlite/sync-client.js",
  "sqlite/hcb1.js",
  "sqlite/crsqlite-sync.mjs",
  "sqlite/crsqlite-sync.wasm",
  "sqlite/wa-sqlite/sqlite-api.js",
  "sqlite/wa-sqlite/sqlite-constants.js",
  "sqlite/wa-sqlite/VFS.js",
  "sqlite/wa-sqlite/examples/AccessHandlePoolVFS.js",
];
let sqliteSynced = 0;
for (const rel of SQLITE_SET) {
  const src = join(srcRoot, rel);
  const dest = join(destRoot, rel);
  if (!existsSync(src)) {
    console.warn(`[sync-browser-node] ${src} missing — the browser-db lane will report unavailable until it exists (run crates/hive-browser/www/sqlite/build-sqlite.sh first).`);
    continue;
  }
  mkdirSync(dirname(dest), { recursive: true });
  copyFileSync(src, dest);
  // Byte-verify every shipped file: a truncated or partial copy would only
  // surface later as an opaque import/wasm failure inside a user's browser,
  // so the sync refuses to be silent about it.
  if (!readFileSync(src).equals(readFileSync(dest))) {
    console.error(`[sync-browser-node] byte mismatch after copy: ${rel}`);
    process.exit(1);
  }
  sqliteSynced++;
}
console.log(`[sync-browser-node] synced ${srcPkg} -> ${destPkg}`);
console.log(`[sync-browser-node] synced sqlite set (${sqliteSynced}/${SQLITE_SET.length} files) -> ${join(destRoot, "sqlite")}`);
