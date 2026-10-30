#!/usr/bin/env bash
# Build vendored node-worker (HeyPuter/node-worker) into crates/hive-browser/www/node-worker/, which
# ui/scripts/sync-browser-node.mjs publishes. A missing pnpm, vendor dir or dist/ exits 0 (skipped).
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
