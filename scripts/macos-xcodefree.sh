#!/usr/bin/env bash
# One shared Xcode-free Darwin build lane.
#
#   scripts/macos-xcodefree.sh --preflight
#   scripts/macos-xcodefree.sh --verify <binary>
#   scripts/macos-xcodefree.sh <command> [args...]
#
# No Xcode and no Command Line Tools are needed. DEVELOPER_DIR is pointed at
# a path that must not exist, so a CLT shim that ignores PATH fails loudly
# instead of opening the "Install Command Line Developer Tools" dialog.
#
# Nothing is removed from the binary: ring, rustls, aws-lc-rs,
# security-framework, sep_signer and the TLS verifier all stay compiled and
# linked. The stub sysroot in ./stubs carries install-name-only .tbd files;
# their real implementations are in the dyld shared cache on every macOS
# host. --verify forces every deferred symbol to bind at launch, so a
# missing symbol fails the build instead of shipping.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)/macos-xcodefree"
CC_SHIM="$HERE/cc"
PY3="${HIVE_PY3:-/opt/homebrew/bin/python3}"
GIT="${HIVE_GIT:-/opt/homebrew/bin/git}"
ZIG="${HIVE_ZIG:-/opt/homebrew/bin/zig}"
LLVM_BIN=/opt/homebrew/opt/llvm/bin
DEV_DIR=/nonexistent

fail() { echo "FATAL: $*" >&2; exit 1; }

preflight() {
  [ "$(uname -s)" = Darwin ] || fail "Darwin only"
  [ ! -e "$DEV_DIR" ] || fail "DEVELOPER_DIR sentinel $DEV_DIR must not exist"
  [ -x "$ZIG" ] || fail "missing $ZIG (brew install zig)"
  [ -x "$PY3" ] || fail "missing $PY3 (brew install python)"
  [ -x "$GIT" ] || fail "missing $GIT (brew install git)"
  [ -x "$LLVM_BIN/llvm-ar" ] || fail "missing $LLVM_BIN/llvm-ar (brew install llvm)"
  [ -x "$LLVM_BIN/llvm-ranlib" ] || fail "missing $LLVM_BIN/llvm-ranlib"
  [ -x "$CC_SHIM" ] || fail "missing $CC_SHIM"
  [ -f "$HERE/stubs/usr/lib/libobjc.tbd" ] || fail "missing stub sysroot $HERE/stubs"
  for t in "$ZIG" "$PY3" "$GIT" "$LLVM_BIN/llvm-ar" "$LLVM_BIN/llvm-ranlib"; do
    case "$t" in /usr/bin/*) fail "$t is a CLT shim" ;; esac
  done
  active="$(DEVELOPER_DIR="$DEV_DIR" xcode-select -p 2>/dev/null || true)"
  case "$active" in
    "$DEV_DIR"|"") ;;
    *) fail "active developer dir is still $active" ;;
  esac
  echo "RESULT PASS xcodefree_preflight dev_dir=$DEV_DIR clt_active=0"
}

verify() {
  bin="$1"
  [ -x "$bin" ] || fail "not executable: $bin"
  err="$(mktemp /private/tmp/hive-xcodefree-verify.XXXXXX)"
  DEVELOPER_DIR="$DEV_DIR" DYLD_BIND_AT_LAUNCH=1 "$bin" --help >/dev/null 2>"$err"
  rc=$?
  if [ "$rc" -ne 0 ] || [ -s "$err" ]; then
    errtext="$(cat "$err")"
    case "$errtext" in
      *"dyld["*|*"Library not loaded"*|*"Symbol not found"*)
        printf '%s\n' "$errtext" >&2; rm -f "$err"
        fail "dyld diagnostic during eager bind" ;;
    esac
  fi
  if [ "$rc" -ne 0 ]; then
    tail -20 "$err" >&2
    rm -f "$err"
    fail "eager symbol bind failed (exit $rc)"
  fi
  rm -f "$err"
  if ! "$PY3" -I - "$bin" <<'PY'
import collections, struct, sys
d = open(sys.argv[1], "rb").read()
if d[:4] not in (b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xca\xfe\xba\xbe"):
    sys.exit("FATAL: not a Mach-O binary")
ncmds = struct.unpack_from("<I", d, 16)[0]
off, names = 32, []
for _ in range(ncmds):
    cmd, size = struct.unpack_from("<II", d, off)
    if cmd in (0xC, 0x80000018, 0x8000001F, 0x20):
        o = struct.unpack_from("<I", d, off + 8)[0]
        names.append(d[off + o:off + size].split(b"\0")[0].decode())
    off += size
dupes = [k for k, v in collections.Counter(names).items() if v > 1]
if dupes:
    sys.exit("FATAL: duplicate load commands: %r" % dupes)
if not any("Security.framework" in x for x in names):
    sys.exit("FATAL: Security.framework link missing (SEP signer / TLS)")
print("RESULT PASS macho_loads=%d security_framework=1 dupes=0" % len(names))
PY
  then fail "Mach-O load-command check failed"; fi
  echo "RESULT PASS xcodefree_verify eager_bind=0 security_framework=1"
}

run_command() {
  export DEVELOPER_DIR="$DEV_DIR"
  unset SDKROOT MACOSX_DEPLOYMENT_TARGET
  # Keep the user's PATH: Homebrew cargo has no cross std. Prepend only.
  export PATH="/opt/homebrew/bin:$LLVM_BIN:$PATH"
  export CC_aarch64_apple_darwin="$CC_SHIM"
  export CC_x86_64_apple_darwin="$CC_SHIM"
  export CC="$CC_SHIM"
  export AR="$LLVM_BIN/llvm-ar"
  export RANLIB="$LLVM_BIN/llvm-ranlib"
  export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$CC_SHIM"
  export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="$CC_SHIM"
  export PY3 GIT ZIG
  exec "$@"
}

case "${1:-}" in
  --preflight) preflight ;;
  --verify)
    shift
    [ "$#" -eq 1 ] || fail "--verify needs exactly one binary path"
    preflight
    verify "$1"
    ;;
  --help|-h)
    echo "usage: $0 --preflight | --verify <binary> | <command> [args...]"
    ;;
  '') fail "no command given (try --preflight or --help)" ;;
  *) preflight; run_command "$@" ;;
esac
