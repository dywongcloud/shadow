# Build the musql browser lane into this directory.
#
# This is a THIRD browser database surface, and it is deliberately not a
# substitute for either of the other two:
#
#   * crates/hive-browser/www/sqlite/  — wa-sqlite + cr-sqlite v0.17 CRR, an
#     OPFS replica per PROJECT, replicating through `Op::CrrSync`.
#   * the managed SQLite lane           — a .db/.musq FILE on a node, reached
#     over Hrana.
#   * THIS                              — musql compiled to wasm, IN MEMORY,
#     inside the page. It has no clock tables, no CRR, no replication and no
#     persistence: `export()` / `open(name, { data })` move a whole database in
#     and out as the bytes of one file.
#
# So this lane exists for analysis/sandbox work over a database snapshot the
# caller already has (an exported .db/.musq), not for tenant state. It is
# pointed at $HIVE_DATA/browser-dbs/* by nothing and must never be.
#
# Why build from source instead of `npm install`: the package is published to
# GitHub Packages, which requires a token even for public packages — a build
# that needs a secret to fetch a dependency is a build that breaks for everyone
# who does not have it.
#
# Output (all in this directory, all gitignored: the wasm is ~23 MB raw):
#   musql.wasm, wasm_exec.js, glue.js, memfs.js, index.js
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="${MUSQL_SRC:-/tmp/musql-probe}"

if [ ! -d "$SRC/npm/wasm" ]; then
  echo "musql source not found at $SRC (set MUSQL_SRC to a musql checkout)" >&2
  exit 1
fi

echo "building musql wasm from $SRC ..."
( cd "$SRC" && GOOS=js GOARCH=wasm GOTOOLCHAIN=auto go build -o "$HERE/musql.wasm" ./npm/wasm )

for f in wasm_exec.js glue.js memfs.js index.js; do
  cp "$SRC/npm/$f" "$HERE/$f"
done

echo "built:"
ls -la "$HERE/musql.wasm" "$HERE/index.js"
echo "musql browser lane ready (serve it compressed: ~5.1 MB gzip / 3.7 MB brotli)"
