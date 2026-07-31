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
WASM_BINDGEN="${WASM_BINDGEN:-wasm-bindgen}"   # must be exactly 0.2.125

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

"$WASM_BINDGEN" --weak-refs --target web --out-dir www/pkg "$ART"

# Optional size pass; skipped if wasm-opt is absent (correctness unaffected).
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt --enable-bulk-memory --enable-nontrapping-float-to-int -Os \
    www/pkg/hive_browser_bg.wasm -o www/pkg/hive_browser_bg.wasm
fi

echo "built www/pkg/ ($(ls -1 www/pkg | tr '\n' ' '))"
