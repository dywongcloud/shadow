#!/bin/bash
# Build the fleet hive-cloud binary, once per glibc group.
#
# Why this script exists. Two failure modes silently shipped a binary that
# could not be debugged, twice:
#
#  1. **The symbol table was lost.** `Cargo.toml` declares
#     `profile.release.strip = "debuginfo"`, but the cross-linked artifact came
#     out with NEITHER `.debug_*` NOR `.symtab` — 29/30 ELF sections, fully
#     stripped (measured 2026-10-09 on two independent builds). A stripped
#     hive-cloud is what made the leader's runtime freezes un-diagnosable
#     (`?? ()` for 25 frames). So the strip value is not trusted: the build
#     runs with `strip = "none"` and DWARF is removed afterwards with
#     `llvm-strip --strip-debug`, and the result is VERIFIED to still carry
#     `.symtab` before it is called an artifact.
#
#  2. **jemalloc linked against a stub archive.** `tikv-jemalloc-sys` built with
#     Apple's `ar` produces a 96-byte `libjemalloc.a`, and the link then fails
#     with `undefined symbol: sdallocx/rallocx/mallocx/mallctl` (or, worse,
#     resolves to nothing). LLVM's `ar`/`ranlib` are required. The 96-byte stub
#     is also why a killed build leaves a poisoned artifact behind: pass
#     --clean-jemalloc after any interrupted build.
#
# Usage:
#   scripts/build-fleet-binary.sh              # both groups
#   scripts/build-fleet-binary.sh 2.39         # one group
#   scripts/build-fleet-binary.sh 2.39 --clean-jemalloc
#
# Output: /tmp/hive-cloud-<glibc>  (plus its sha256)
set -euo pipefail

LLVM_BIN=/opt/homebrew/opt/llvm/bin
AR_BIN="$LLVM_BIN/llvm-ar"
RANLIB_BIN="$LLVM_BIN/llvm-ranlib"
STRIP_BIN="$LLVM_BIN/llvm-strip"

for tool in "$AR_BIN" "$RANLIB_BIN" "$STRIP_BIN"; do
  [ -x "$tool" ] || { echo "FATAL: missing $tool (brew install llvm)"; exit 1; }
done

GROUPS=("2.38" "2.39")
if [ "${1:-}" = "2.38" ] || [ "${1:-}" = "2.39" ]; then GROUPS=("$1"); fi
CLEAN=0
for a in "$@"; do [ "$a" = "--clean-jemalloc" ] && CLEAN=1; done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

has_symtab() { # $1 path -> 0 when .symtab is present
  python3 - "$1" <<'PY'
import struct, sys
p = sys.argv[1]
with open(p, "rb") as f:
    h = f.read(64)
    shoff, = struct.unpack_from("<Q", h, 0x28)
    shent, shnum, shstrndx = struct.unpack_from("<HHH", h, 0x3a)
    f.seek(shoff); sh = f.read(shent * shnum)
def shdr(i):
    return struct.unpack_from("<IIQQQQIIQQ", sh, i * shent)
off, size = shdr(shstrndx)[4], shdr(shstrndx)[5]
with open(p, "rb") as f:
    f.seek(off); st = f.read(size)
names = [st[shdr(i)[0]:st.index(b"\0", shdr(i)[0])].decode() for i in range(shnum)]
sym = ".symtab" in names
dbg = sum("debug" in n for n in names)
print(f"  sections={len(names)} symtab={sym} debug_sections={dbg}")
sys.exit(0 if sym and dbg == 0 else 1)
PY
}

for G in "${GROUPS[@]}"; do
  TRIPLE="x86_64-unknown-linux-gnu.$G"
  echo "== building glibc $G ($TRIPLE) =="
  if [ "$CLEAN" = 1 ]; then
    rm -rf "target/$TRIPLE/release/build/tikv-jemalloc-sys-"*
    echo "  cleaned tikv-jemalloc-sys artifacts"
  fi
  AR="$AR_BIN" RANLIB="$RANLIB_BIN" CARGO_PROFILE_RELEASE_STRIP=none \
    cargo zigbuild --release --target "$TRIPLE" -p hive-cloud

  SRC="target/$TRIPLE/release/hive-cloud"
  OUT="/tmp/hive-cloud-$G"
  [ -f "$SRC" ] || { echo "FATAL: no artifact at $SRC"; exit 1; }
  cp "$SRC" "$OUT"

  echo "post-link:"; has_symtab "$OUT" || { echo "FATAL: no .symtab after link"; exit 1; }
  "$STRIP_BIN" --strip-debug "$OUT"
  echo "after llvm-strip --strip-debug:"; has_symtab "$OUT" \
    || { echo "FATAL: .symtab lost while stripping DWARF"; exit 1; }

  echo "  artifact $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes)"
  echo "  sha256   $(shasum -a 256 "$OUT" | cut -d' ' -f1)"
done
echo "FLEET_BUILD_DONE"
