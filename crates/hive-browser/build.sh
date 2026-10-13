#!/usr/bin/env bash
# Build the hive-browser wasm node + its JS bindings into www/pkg/.
set -euo pipefail
cd "$(dirname "$0")"

TC="${RUSTUP_TC:-$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin}"
LLVM="${LLVM_BIN:-/opt/homebrew/opt/llvm/bin}"

WB_PIN="$(sed -n 's/^wasm-bindgen = "=\(.*\)"$/\1/p' Cargo.toml)"
[ -n "$WB_PIN" ] || { echo "FAIL: could not read the wasm-bindgen pin from Cargo.toml" >&2; exit 1; }

if [ -z "${WASM_BINDGEN:-}" ]; then
  if command -v wasm-bindgen >/dev/null 2>&1; then WASM_BINDGEN="$(command -v wasm-bindgen)"
  elif [ -x "$HOME/.cargo/bin/wasm-bindgen" ]; then WASM_BINDGEN="$HOME/.cargo/bin/wasm-bindgen"
  else
    echo "FAIL: wasm-bindgen CLI not found (looked on PATH and in ~/.cargo/bin)." >&2
    echo "      cargo install wasm-bindgen-cli --version $WB_PIN" >&2
    exit 1
  fi
fi

WB_HAVE="$("$WASM_BINDGEN" --version 2>/dev/null | awk '{print $2}')"
if [ "$WB_HAVE" != "$WB_PIN" ]; then
  echo "FAIL: wasm-bindgen CLI is ${WB_HAVE:-unknown}, but Cargo.toml pins $WB_PIN." >&2
  echo "      A mismatch is a hard runtime error, not a warning. Install the pin:" >&2
  echo "      cargo install wasm-bindgen-cli --version $WB_PIN" >&2
  exit 1
fi

env RUSTC="$TC/rustc" PATH="$TC:$PATH" \
    CC_wasm32_unknown_unknown="$LLVM/clang" \
    AR_wasm32_unknown_unknown="$LLVM/llvm-ar" \
    "$TC/cargo" build --target wasm32-unknown-unknown --release

ART=target/wasm32-unknown-unknown/release/hive_browser.wasm

if command -v wasm-tools >/dev/null 2>&1; then
  if wasm-tools print --skeleton "$ART" | grep -q 'import "env"'; then
    echo "FAIL: wasm imports from \"env\" (unsatisfiable host functions)" >&2
    exit 1
  fi
fi

OUT=www/pkg
TMP=www/pkg.next
OLD=www/pkg.old
rm -rf "$TMP" "$OLD"
restore_bundle() {
  rm -rf "$TMP"
  if [ -d "$OLD" ] && [ ! -e "$OUT" ]; then mv "$OLD" "$OUT"; fi
}
trap restore_bundle ERR INT TERM
"$WASM_BINDGEN" --weak-refs --target web --out-dir "$TMP" "$ART"

if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt --enable-bulk-memory --enable-nontrapping-float-to-int -Os \
    "$TMP/hive_browser_bg.wasm" -o "$TMP/hive_browser_bg.wasm"
fi

if [ ! -f www/node-worker/worker.js ] || [ ! -f www/node-worker/index.js ] || [ ! -f www/node-worker/sw.js ]; then
  echo "WARN: www/node-worker/ is unbuilt — browser execution will report node_worker_unbuilt until scripts/build-node-worker.sh runs." >&2
fi

if [ -d "$OUT" ]; then mv "$OUT" "$OLD"; fi
mv "$TMP" "$OUT"
rm -rf "$OLD"
trap - ERR INT TERM

echo "built $OUT/ ($(ls -1 "$OUT" | tr '\n' ' '))"
