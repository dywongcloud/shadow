#!/usr/bin/env bash
# Build the browser crsqlite wasm artifact (SQLite 3.45.0 + the vendored
# superfly/cr-sqlite v0.17 CRR extension, statically linked) via the wa-sqlite
# Emscripten tooling, for bn-impl-sqlite-automerge Phase A.
#
# Layout mandated by the PRD row (do not "improve" away):
#   * emsdk PINNED at 3.1.45 -- its LLVM 17 matches the pinned Rust nightly's
#     LLVM 17, which is what makes the Rust LLVM bitcode linkable by emcc.
#   * Rust PINNED at nightly-2023-10-05 + rust-src (cr-sqlite's own
#     rs/*/rust-toolchain.toml pin; #![feature(lang_items)] is not on stable;
#     rust-src is needed for -Z build-std).
#   * wa-sqlite (the vlcn-io fork, whose Makefile carries the crsqlite
#     targets) PINNED at the commit below. Its `crsql` symlink MUST target
#     vendor/cr-sqlite/core AS A WHOLE -- the Makefile consumes BOTH
#     crsql/src (C extension files) and crsql/rs/bundle (Rust static-lib
#     bitcode). Never point it at core/rs/core, and never substitute the
#     @vlcn.io/crsqlite-wasm 0.16 npm package (wire-incompatible with v0.17:
#     it lacks the ts column).
#   * Rust features: static,omit_load_extension -- the `test` feature stays
#     OFF (it switches the panic handler and pulls test-only exports).
#   * RUSTFLAGS PINNED: "--emit=llvm-bc -C linker=/usr/bin/true" (from the
#     wa-sqlite Makefile): emit LLVM bitcode for emcc to LTO-link, never
#     invoke a linker from rustc.
#
# Outputs (committed):
#   crsqlite-sync.mjs / crsqlite-sync.wasm  -- browser artifact, ENVIRONMENT
#     web,worker only (DedicatedWorker-hosted; see sqlite-worker.js).
#   wa-sqlite/*.js, wa-sqlite/examples/AccessHandlePoolVFS.js -- the matching
#     runtime JS, copied from the pinned checkout (no CDN, no npm runtime dep).
# Outputs (gitignored, .build/):
#   proof-node/crsqlite-sync.{mjs,wasm} -- the SAME build with node added to
#     ENVIRONMENT so prove-wire.sh can execute the wasm side of the
#     ten-column wire proof under Node.
#
# Mirrors crates/hive-browser/build.sh's discipline: every pin is gated
# BEFORE compiling, failures are loud with the remediation printed.
set -euo pipefail
cd "$(dirname "$0")"

EMSDK_PIN=3.1.45
RUST_PIN=nightly-2023-10-05
# vlcn-io/wa-sqlite master tip, 2024-01-17 ("sqlite 3.45").
WA_SQLITE_REPO=https://github.com/vlcn-io/wa-sqlite.git
WA_SQLITE_PIN=232f21ae4b89972ca70f999554bb39a8ddc9a853
# The vendored superfly/cr-sqlite commit (vendor/cr-sqlite/VENDOR.md). Passed
# on the make command line because the Makefile's default (`cd ..; git
# rev-parse HEAD`) would embed the HIVE repo's sha from this layout.
CRSQL_COMMIT=ec0d669daa9a051d4c6f4a4d9c653eac40e7a437

REPO_ROOT="$(cd ../../../.. && pwd)"
CRSQL_CORE="$REPO_ROOT/vendor/cr-sqlite/core"
BUILD=.build
WA="$BUILD/wa-sqlite"

# --- gate 1: emsdk at the pin -------------------------------------------------
EMSDK="${EMSDK:-$HOME/emsdk}"
[ -f "$EMSDK/emsdk_env.sh" ] || {
  echo "FAIL: emsdk not found at $EMSDK (set \$EMSDK)." >&2
  echo "      git clone https://github.com/emscripten-core/emsdk.git ~/emsdk" >&2
  echo "      ~/emsdk/emsdk install $EMSDK_PIN && ~/emsdk/emsdk activate $EMSDK_PIN" >&2
  echo "      (user-level install only -- never system-wide)" >&2
  exit 1
}
# emsdk_env.sh clobbers PATH for the subshell; keep it scoped to this script.
source "$EMSDK/emsdk_env.sh" >/dev/null 2>&1
# First semver token on the version line -- robust across `3.1.45 (<sha>)`
# (emsdk) and `5.0.6-git` (homebrew) formats.
EMCC_HAVE="$(emcc --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
[ "$EMCC_HAVE" = "$EMSDK_PIN" ] || {
  echo "FAIL: emcc is ${EMCC_HAVE:-unknown}, pinned emsdk is $EMSDK_PIN." >&2
  echo "      The Rust-nightly LLVM must match emscripten's LLVM for the" >&2
  echo "      bitcode link; this is a hard requirement, not a warning." >&2
  echo "      $EMSDK/emsdk install $EMSDK_PIN && $EMSDK/emsdk activate $EMSDK_PIN" >&2
  exit 1
}

# --- gate 2: pinned Rust nightly + rust-src -----------------------------------
command -v rustup >/dev/null 2>&1 || {
  echo "FAIL: rustup not found -- required for the pinned $RUST_PIN toolchain." >&2
  exit 1
}
rustup toolchain list | grep -q "^$RUST_PIN" || {
  echo "FAIL: $RUST_PIN not installed (rustup toolchain install $RUST_PIN)." >&2
  exit 1
}
rustup component list --toolchain "$RUST_PIN" 2>/dev/null | grep -q "^rust-src (installed)" || {
  echo "FAIL: rust-src missing for $RUST_PIN (needed for -Z build-std):" >&2
  echo "      rustup component add rust-src --toolchain $RUST_PIN" >&2
  exit 1
}

# --- gate 3: node (wire proof only) + curl + tclsh (sqlite amalgamation) ------
for tool in node curl tclsh cc make; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "FAIL: $tool not found on PATH (required)." >&2
    exit 1
  }
done

# --- gate 4: wa-sqlite checkout at the pin ------------------------------------
if [ ! -d "$WA/.git" ]; then
  mkdir -p "$BUILD"
  git clone "$WA_SQLITE_REPO" "$WA"
fi
git -C "$WA" cat-file -e "$WA_SQLITE_PIN^{commit}" 2>/dev/null || git -C "$WA" fetch origin "$WA_SQLITE_PIN"
git -C "$WA" checkout --quiet "$WA_SQLITE_PIN"
HAVE="$(git -C "$WA" rev-parse HEAD)"
[ "$HAVE" = "$WA_SQLITE_PIN" ] || {
  echo "FAIL: wa-sqlite checkout is at $HAVE, pinned at $WA_SQLITE_PIN." >&2
  exit 1
}

# --- the crsql symlink: whole core, never core/rs/core -------------------------
[ -f "$CRSQL_CORE/src/crsqlite.c" ] && [ -f "$CRSQL_CORE/rs/bundle/Cargo.toml" ] || {
  echo "FAIL: $CRSQL_CORE does not look like the vendored cr-sqlite core" >&2
  echo "      (need both src/ and rs/bundle -- see vendor/cr-sqlite/VENDOR.md)." >&2
  exit 1
}
rm -f "$WA/crsql"
ln -s "$CRSQL_CORE" "$WA/crsql"
[ -f "$WA/crsql/src/crsqlite.c" ] && [ -f "$WA/crsql/rs/bundle/Cargo.toml" ] || {
  echo "FAIL: crsql symlink does not resolve to BOTH src/ and rs/bundle." >&2
  exit 1
}

# --- build ---------------------------------------------------------------------
# SQLite 3.45.0 amalgamation + sha-checked extension-functions (network on
# first run; cached under $BUILD/wa-sqlite/{cache,deps} afterwards). The deps
# are built explicitly because the Makefile's `deps` phony is empty upstream.
make -C "$WA" deps/version-3.45.0/sqlite3-extra.c deps/extension-functions.c

# Browser artifact (ENVIRONMENT web,worker). The Makefile reruns the pinned
# cargo bitcode build itself (RUSTFLAGS and features are in the Makefile);
# CRSQLITE_COMMIT_SHA is overridden on the command line (see header).
make -C "$WA" dist/crsqlite-sync.mjs CRSQLITE_COMMIT_SHA="$CRSQL_COMMIT"
cp "$WA/dist/crsqlite-sync.mjs" "$WA/dist/crsqlite-sync.wasm" .

# Node-capable variant of the SAME build for the wire proof.
make -C "$WA" dist/crsqlite-sync.mjs \
  EMFLAGS_EXTRA='-sENVIRONMENT=web,worker,node' \
  CRSQLITE_COMMIT_SHA="$CRSQL_COMMIT"
mkdir -p "$BUILD/proof-node"
cp "$WA/dist/crsqlite-sync.mjs" "$WA/dist/crsqlite-sync.wasm" "$BUILD/proof-node/"

# Runtime JS matching the pinned checkout (the worker imports these).
mkdir -p wa-sqlite/examples
cp "$WA/src/sqlite-api.js" "$WA/src/sqlite-constants.js" "$WA/src/VFS.js" wa-sqlite/
cp "$WA/src/examples/AccessHandlePoolVFS.js" wa-sqlite/examples/

# --- post-build verification gates ---------------------------------------------
# crsql must actually be in the committed wasm.
if ! grep -q "crsql_changes" crsqlite-sync.wasm; then
  echo "FAIL: crsqlite-sync.wasm has no crsql symbols -- the Rust bitcode" >&2
  echo "      half did not make it into the link." >&2
  exit 1
fi
# The committed artifact is browser-only; node support belongs to the
# (gitignored) proof build. If this trips, EMFLAGS_EXTRA leaked between runs.
if grep -qE "process\.versions|node:fs|readFileSync" crsqlite-sync.mjs; then
  echo "FAIL: committed crsqlite-sync.mjs contains node environment code" >&2
  echo "      (expected ENVIRONMENT=web,worker only)." >&2
  exit 1
fi
if ! grep -qE "process\.versions|node:fs|readFileSync" "$BUILD/proof-node/crsqlite-sync.mjs"; then
  echo "FAIL: proof-node build is missing node environment support" >&2
  echo "      (prove-wire.sh cannot run without it)." >&2
  exit 1
fi

echo "OK: crsqlite-sync.{mjs,wasm} (browser) + .build/proof-node/ (wire proof)"
echo "    crsqlite wasm: $(wc -c < crsqlite-sync.wasm | tr -d ' ') bytes, crsql commit $CRSQL_COMMIT"
echo "    next: ./prove-wire.sh   -- ten-column crsql_changes wire proof vs hive-crsql"
