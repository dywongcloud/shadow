#!/usr/bin/env bash
# Builds vendored node-worker (HeyPuter/node-worker) into the ES modules the
# browser node ships, and stages them where the dashboard sync picks them up.
#
# WHY THIS EXISTS. node-worker is not a drop-in JS file: its `build` runs
# `prepare-node.js` FIRST, which shallow-clones the real Node.js source
# (nodejs/node at the commit pinned in vendor/node-worker/package.json ->
# `nodeCore.commit`, currently v25.9.0) into `node_core/`, then rollup bundles
# that into dist/{index,worker,sw,sw-handler}.js. The Node core is what makes
# this a genuine Node runtime rather than a shim: `node:async_hooks` (and so
# AsyncLocalStorage), `node:net`, `node:tls` and the rest are Node's own lib
# transpiled for a Worker -- which is also why the build is heavy and why the
# output must be a committed/synced artifact rather than an npm install.
#
# Output is copied to crates/hive-browser/www/node-worker/ so
# ui/scripts/sync-browser-node.mjs publishes it to
# ui/public/browser-node/node-worker/ exactly like every other browser-node
# asset. A missing/failed build is a WARN, never a hard failure of the caller.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NW="$ROOT/vendor/node-worker"
OUT="$ROOT/crates/hive-browser/www/node-worker"

if ! command -v pnpm >/dev/null 2>&1; then
  echo "[build-node-worker] pnpm not installed — skipping (node-worker lane stays unbuilt)." >&2
  exit 0
fi
if [ ! -d "$NW" ]; then
  echo "[build-node-worker] $NW missing — skipping." >&2
  exit 0
fi

echo "[build-node-worker] installing deps…"
(cd "$NW" && pnpm install --frozen-lockfile)

echo "[build-node-worker] prepare-node (clones Node core — large, first run only)…"
(cd "$NW" && pnpm run prepare-node)

echo "[build-node-worker] rollup build…"
(cd "$NW" && pnpm run build)

if [ ! -d "$NW/dist" ]; then
  echo "[build-node-worker] no dist/ produced — skipping copy." >&2
  exit 0
fi

mkdir -p "$OUT"
for f in index.js worker.js sw.js sw-handler.js; do
  if [ -f "$NW/dist/$f" ]; then
    cp -f "$NW/dist/$f" "$OUT/$f"
  else
    echo "[build-node-worker] warning: dist/$f not produced." >&2
  fi
done

echo "[build-node-worker] staged into $OUT"
