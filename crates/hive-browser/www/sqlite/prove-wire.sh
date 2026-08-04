#!/usr/bin/env bash
# Live wire-format proof for bn-impl-sqlite-automerge: the ten-column
# crsql_changes v0.17 wire (INCLUDING ts) round-trips between the fleet's
# native sync path (hive-crsql, vendored superfly/cr-sqlite dylib) and the
# real browser crsqlite wasm build (same vendored source, wa-sqlite
# Emscripten tooling) running under Node.
#
# This is sync_roundtrip with peer B swapped for the actual wasm artifact:
#   1. native peer A: local write -> a.json          (cargo example wire_proof)
#   2. wasm   peer B: local write -> b.json, then applies a.json -> b-final.json
#   3. native peer A: applies b.json -> a-final.json
#   4. compare: changes that crossed the wire must arrive byte-identical in
#      ALL TEN columns (ts included); both peers must converge on the same
#      row set. Any mismatch exits non-zero with WIRE_PROOF_FAIL.
#
# Prerequisites: vendor/cr-sqlite built (vendor/cr-sqlite/build.sh) and the
# node-capable proof build present (build-sqlite.sh produces .build/proof-node/).
set -euo pipefail
cd "$(dirname "$0")"

REPO_ROOT=../../../..
WORK=.build/wire-proof
rm -rf "$WORK"
mkdir -p "$WORK"

[ -f "$REPO_ROOT/vendor/cr-sqlite/core/dist/crsqlite.dylib" ] || \
[ -f "$REPO_ROOT/vendor/cr-sqlite/core/dist/crsqlite.so" ] || {
  echo "FAIL: vendored crsql loadable extension missing -- run vendor/cr-sqlite/build.sh first" >&2
  exit 1
}
[ -f .build/proof-node/crsqlite-sync.mjs ] || {
  echo "FAIL: node-capable proof build missing -- run build-sqlite.sh first" >&2
  exit 1
}

echo "== 1/4 native peer A: local write, export a.json"
(cd "$REPO_ROOT" && cargo run -q -p hive-crsql --example wire_proof -- \
  export-a "$OLDPWD/$WORK/a.db" "$OLDPWD/$WORK/a.json")

echo "== 2/4 wasm peer B (node): local write, export b.json, apply a.json"
node wire-proof-node.mjs run-b "$WORK/a.json" "$WORK/b.json" "$WORK/b-final.json"

echo "== 3/4 native peer A: apply b.json, dump a-final.json"
(cd "$REPO_ROOT" && cargo run -q -p hive-crsql --example wire_proof -- \
  apply-final "$OLDPWD/$WORK/a.db" "$OLDPWD/$WORK/b.json" "$OLDPWD/$WORK/a-final.json")

echo "== 4/4 compare: ten-column wire agreement incl. ts + row convergence"
node wire-proof-node.mjs compare \
  "$WORK/a.json" "$WORK/b.json" "$WORK/a-final.json" "$WORK/b-final.json"

# bn-browser-fleet-crr-exchange extension: the canonical HCB1 batch frames the
# exchange actually rides, proven across the SAME two runtimes — the browser
# glue's hcb1.js against hive_crsql's ChangeBatch::encode/decode.
echo "== 5/8 HCB1: native export -> JS decode+re-encode -> byte-compare"
(cd "$REPO_ROOT" && cargo run -q -p hive-crsql --example wire_proof -- \
  export-hcb1 "$OLDPWD/$WORK/a.db" "$OLDPWD/$WORK/a.hcb1.hex")
node hcb1-proof-node.mjs roundtrip "$WORK/a.hcb1.hex" "$WORK/a.hcb1.roundtrip.hex"
cmp "$WORK/a.hcb1.hex" "$WORK/a.hcb1.roundtrip.hex" || {
  echo "WIRE_PROOF_FAIL: hcb1.js round-trip mutated Rust-encoded frames" >&2
  exit 1
}

echo "== 6/8 HCB1: JS encode from the wasm export -> native decode/re-encode/apply"
node hcb1-proof-node.mjs emit "$WORK/b.json" "$WORK/b.hcb1.hex"
rm -f "$WORK/c.db"
(cd "$REPO_ROOT" && cargo run -q -p hive-crsql --example wire_proof -- \
  apply-hcb1 "$OLDPWD/$WORK/c.db" "$OLDPWD/$WORK/b.hcb1.hex" "$OLDPWD/$WORK/b.hcb1.rust.hex")
cmp "$WORK/b.hcb1.hex" "$WORK/b.hcb1.rust.hex" || {
  echo "WIRE_PROOF_FAIL: hive-crsql re-encode mutated JS-encoded frames" >&2
  exit 1
}

echo "== 7/8 HCB1: byte-identical both directions"

echo "== 8/8 HCB1 wire agreement proven"
