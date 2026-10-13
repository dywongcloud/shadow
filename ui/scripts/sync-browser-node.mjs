#!/usr/bin/env node
// Syncs the built hive-browser bundle and the hand-written browser-node modules from
// crates/hive-browser/www into ui/public/browser-node; a missing artifact is a WARN, never a
// build failure ("Run a node" reports its own missing-bundle state at runtime).
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
for (const f of [
  "identity.js",
  "artifact-policy.js",
  "worker-function-runtime.js",
  "peer-mesh.js",
  "peer-mesh-agent.js",
  "sync-fs-agent.js",
  "node-worker-host.js",
  "node-worker-agent.js",
  "node-worker-vfs.js",
]) {
  const src = join(srcRoot, f);
  if (existsSync(src)) {
    copyFileSync(src, join(destRoot, f));
  } else if (f !== "identity.js") {
    console.warn(`[sync-browser-node] ${src} missing — the browser-node lane it belongs to will fail to load until it exists.`);
  }
}

const NODE_WORKER_SET = ["index.js", "worker.js", "sw.js", "sw-handler.js"];
const nwSrcRoot = join(srcRoot, "node-worker");
if (existsSync(nwSrcRoot)) {
  const nwDest = join(destRoot, "node-worker");
  mkdirSync(nwDest, { recursive: true });
  for (const f of NODE_WORKER_SET) {
    const s = join(nwSrcRoot, f);
    if (existsSync(s)) {
      copyFileSync(s, join(nwDest, f));
    } else {
      console.warn(
        `[sync-browser-node] ${s} missing — the node-worker lane will report itself unbuilt (run scripts/build-node-worker.sh).`,
      );
    }
  }
} else {
  console.warn(
    `[sync-browser-node] ${nwSrcRoot} not built yet — skipping node-worker assets (run scripts/build-node-worker.sh).`,
  );
}

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
  if (!readFileSync(src).equals(readFileSync(dest))) {
    console.error(`[sync-browser-node] byte mismatch after copy: ${rel}`);
    process.exit(1);
  }
  sqliteSynced++;
}
console.log(`[sync-browser-node] synced ${srcPkg} -> ${destPkg}`);
console.log(`[sync-browser-node] synced sqlite set (${sqliteSynced}/${SQLITE_SET.length} files) -> ${join(destRoot, "sqlite")}`);

const MUSQL_SET = [
  "musql/musql-worker.js",
  "musql/musql-client.js",
  "musql/index.js",
  "musql/glue.js",
  "musql/memfs.js",
  "musql/wasm_exec.js",
  "musql/musql.wasm",
];
let musqlSynced = 0;
for (const rel of MUSQL_SET) {
  const src = join(srcRoot, rel);
  const dest = join(destRoot, rel);
  if (!existsSync(src)) {
    console.warn(
      `[sync-browser-node] ${src} missing — the musql browser lane will report unavailable until it exists (run crates/hive-browser/www/musql/build-musql.sh first).`,
    );
    continue;
  }
  mkdirSync(dirname(dest), { recursive: true });
  copyFileSync(src, dest);
  if (!readFileSync(src).equals(readFileSync(dest))) {
    console.error(`[sync-browser-node] byte mismatch after copy: ${rel}`);
    process.exit(1);
  }
  musqlSynced++;
}
console.log(
  `[sync-browser-node] synced musql set (${musqlSynced}/${MUSQL_SET.length} files) -> ${join(destRoot, "musql")}`,
);
