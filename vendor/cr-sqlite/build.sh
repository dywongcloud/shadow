#!/usr/bin/env bash
# Build the vendored superfly/cr-sqlite loadable SQLite extension.
# See VENDOR.md for provenance + why this fork.
set -euo pipefail
cd "$(dirname "$0")"

# core/rs/*/rust-toolchain.toml pins nightly-2023-10-05 (inherited from
# upstream — bundle's cdylib entry point uses #![feature(lang_items)], not on
# stable). rustup auto-installs it on first invocation of `cargo` inside those
# directories; fail loud up front if rustup itself isn't present at all,
# rather than letting `make` fail deep inside a submodule build with a less
# legible error.
if ! command -v rustup >/dev/null 2>&1; then
  echo "FAIL: rustup not found — required to auto-install the pinned nightly-2023-10-05 toolchain this vendor's core/rs/*/rust-toolchain.toml files declare." >&2
  exit 1
fi

make crsqlite

case "$(uname -s)" in
  Darwin) EXT=dylib ;;
  Linux) EXT=so ;;
  *) EXT="" ;;
esac
OUT="core/dist/crsqlite.${EXT:-so}"
if [ ! -f "$OUT" ]; then
  echo "FAIL: expected build output missing: $OUT" >&2
  exit 1
fi
echo "OK: $OUT"
