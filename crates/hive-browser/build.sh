#!/usr/bin/env bash
# Build the hive-browser wasm node + its JS bindings into www/pkg/.
#
# Why the env gymnastics: on a machine where Homebrew's rust shadows the
# rustup toolchain in PATH, `cargo` resolves `rustc` by PATH and picks the
# Homebrew rustc, which ships no wasm32 std ("can't find crate for core").
# Force the rustup toolchain's own rustc, and point ring's C compilation at an
# LLVM clang that actually has a WebAssembly backend (Apple's system clang does
# not) — otherwise `tls-ring` fails building curve25519.c for wasm32.
set -euo pipefail
cd "$(dirname "$0")"

TC="${RUSTUP_TC:-$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin}"
LLVM="${LLVM_BIN:-/opt/homebrew/opt/llvm/bin}"

# wasm-bindgen CLI must match the wasm-bindgen dep pin EXACTLY — a mismatch is a
# hard runtime error ("invalid schema version"), not a warning. `cargo install`
# drops it in ~/.cargo/bin, which is not necessarily on PATH (it isn't on this
# machine), so look there before giving up.
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

# Gate BEFORE compiling. Discovering the CLI is missing or wrong only after the
# cargo build leaves a freshly-built .wasm next to a STALE www/pkg/ bundle from
# an earlier run — which still loads, so a browser witness can appear to pass
# against code that was never rebuilt. Fail before producing that state.
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

# Leak guard (adopted from iroh-blobs' own CI): a browser wasm module must not
# import from "env" — that would mean a host function it can't satisfy.
if command -v wasm-tools >/dev/null 2>&1; then
  if wasm-tools print --skeleton "$ART" | grep -q 'import "env"'; then
    echo "FAIL: wasm imports from \"env\" (unsatisfiable host functions)" >&2
    exit 1
  fi
fi

# Build every browser artifact into a sibling directory. A failed Rust, bindgen,
# npm, or esbuild stage leaves the last complete bundle intact; publishing is the
# only point that touches www/pkg.
command -v npm >/dev/null 2>&1 || { echo "FAIL: npm is required for the function runtime bundle" >&2; exit 1; }
npm ci --ignore-scripts --no-audit --no-fund

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
npm run --silent build:runtime -- --outfile="$TMP/function-worker.js"

# Optional size pass; skipped if wasm-opt is absent (correctness unaffected).
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt --enable-bulk-memory --enable-nontrapping-float-to-int -Os \
    "$TMP/hive_browser_bg.wasm" -o "$TMP/hive_browser_bg.wasm"
fi

# The embedded release-sync interpreter is ~500 KiB before JS glue. Catch a
# loader/config regression that silently turns it back into a network fetch.
[ "$(wc -c < "$TMP/function-worker.js")" -gt 600000 ] || {
  echo "FAIL: function worker is missing embedded QuickJS wasm" >&2
  exit 1
}

if [ -d "$OUT" ]; then mv "$OUT" "$OLD"; fi
mv "$TMP" "$OUT"
rm -rf "$OLD"
trap - ERR INT TERM

echo "built $OUT/ ($(ls -1 "$OUT" | tr '\n' ' '))"
