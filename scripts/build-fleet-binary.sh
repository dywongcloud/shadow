#!/bin/bash
# Build the fleet hive-cloud binary, once per glibc group, with the symbol table verified present:
# the cross-linked artifact loses .symtab despite `strip = "debuginfo"`, so the build runs with
# strip = none, DWARF is removed with `llvm-strip --strip-debug`, and `.symtab` is checked after.
# jemalloc needs LLVM ar/ranlib (Apple's ar yields a 96-byte stub libjemalloc.a); pass
# --clean-jemalloc after any interrupted build.
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

TARGET_GROUPS=("2.38" "2.39")
if [ "${1:-}" = "2.38" ] || [ "${1:-}" = "2.39" ]; then TARGET_GROUPS=("$1"); fi
CLEAN=0
for a in "$@"; do [ "$a" = "--clean-jemalloc" ] && CLEAN=1; done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# elf_check <path> linked|stripped: .symtab required; "stripped" also requires no .debug_* left.
elf_check() {
  python3 - "$1" "$2" <<'PY'
import struct, sys
p, mode = sys.argv[1], sys.argv[2]
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
# The two modes differ on purpose. Right after the link DWARF is still there
# (we asked for strip=none), so demanding zero .debug_* there is a check that
# can only ever fail -- it rejected a healthy 40-section build with
# "FATAL: no .symtab after link" while `.symtab` was in fact present
# (2026-10-09). The post-link gate is one thing only: the symbol table exists.
ok = sym if mode == "linked" else (sym and dbg == 0)
print(f"  sections={len(names)} symtab={sym} debug_sections={dbg} ({mode})")
sys.exit(0 if ok else 1)
PY
}

for G in "${TARGET_GROUPS[@]}"; do
  TRIPLE="x86_64-unknown-linux-gnu.$G"
  echo "== building glibc $G ($TRIPLE) =="
  if [ "$CLEAN" = 1 ]; then
    rm -rf "target/$TRIPLE/release/build/tikv-jemalloc-sys-"*
    echo "  cleaned tikv-jemalloc-sys artifacts"
  fi
  # Both glibc groups build into the same x86_64-unknown-linux-gnu target dir and zig applies the
  # glibc version at LINK time: force a relink of the bin crate between groups, or the previous
  # group's binary ships under this group's name.
  rm -f "target/x86_64-unknown-linux-gnu/release/hive-cloud"
  touch crates/hive-cloud/src/main.rs

  AR="$AR_BIN" RANLIB="$RANLIB_BIN" CARGO_PROFILE_RELEASE_STRIP=none \
    cargo zigbuild --release --target "$TRIPLE" -p hive-cloud

  SRC="target/x86_64-unknown-linux-gnu/release/hive-cloud"
  OUT="/tmp/hive-cloud-$G"
  [ -f "$SRC" ] || { echo "FATAL: no artifact at $SRC"; exit 1; }
  cp "$SRC" "$OUT"

  echo "post-link:"; elf_check "$OUT" linked \
    || { echo "FATAL: no .symtab after link"; exit 1; }
  "$STRIP_BIN" --strip-debug "$OUT"
  echo "after llvm-strip --strip-debug:"; elf_check "$OUT" stripped \
    || { echo "FATAL: .symtab lost while stripping DWARF"; exit 1; }

  echo "  artifact $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes)"
  echo "  sha256   $(shasum -a 256 "$OUT" | cut -d' ' -f1)"
done
echo "FLEET_BUILD_DONE"
