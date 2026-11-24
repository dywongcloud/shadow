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

# Everything --verify needs on a bare fleet Mac: base macOS only. A prebuilt
# deployment has no compiler, so it must NOT require zig/LLVM/git/Homebrew
# python to be present.
preflight_common() {
  [ "$(uname -s)" = Darwin ] || fail "Darwin only"
  [ ! -e "$DEV_DIR" ] || fail "DEVELOPER_DIR sentinel $DEV_DIR must not exist"
  active="$(DEVELOPER_DIR="$DEV_DIR" xcode-select -p 2>/dev/null || true)"
  case "$active" in
    "$DEV_DIR"|"") ;;
    *) fail "active developer dir is still $active" ;;
  esac
  echo "RESULT PASS xcodefree_preflight dev_dir=$DEV_DIR clt_active=0"
}

# The build lane additionally needs the Homebrew toolchain. Only --preflight
# and command mode call this.
preflight_build() {
  preflight_common
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
  echo "RESULT PASS xcodefree_build_tools clt_active=0"
}

verify() {
  bin="$1"
  [ -x "$bin" ] || fail "not executable: $bin"
  # Mach-O magic, load-command check and eager bind use only base-OS tools;
  # no interpreter is required on the CLT-free fleet Mac. dyld itself aborts
  # on a duplicate LC_LOAD_DYLIB, seen live as "duplicate linked dylib".
  magic="$(od -An -tx1 -N4 "$bin" | tr -d ' \n')"
  case "$magic" in
    cffaedfe|cefaedfe|cafebabe) ;;
    *) fail "not a Mach-O binary (magic=$magic)" ;;
  esac
  LC_ALL=C grep -a -q 'Security.framework/Versions/A/Security' "$bin" \
    || fail "Security.framework link missing (SEP signer / TLS)"
  err="$(mktemp /private/tmp/hive-xcodefree-verify.XXXXXX)"
  DEVELOPER_DIR="$DEV_DIR" DYLD_BIND_AT_LAUNCH=1 "$bin" --help >/dev/null 2>"$err"
  rc=$?
  errtext="$(cat "$err")"
  case "$errtext" in
    *"dyld["*|*"Library not loaded"*|*"Symbol not found"*)
      printf '%s\n' "$errtext" >&2; rm -f "$err"
      fail "dyld diagnostic during eager bind" ;;
  esac
  if [ "$rc" -ne 0 ]; then
    printf '%s\n' "$errtext" >&2
    rm -f "$err"
    fail "eager symbol bind failed (exit $rc)"
  fi
  rm -f "$err"
  echo "RESULT PASS xcodefree_verify macho=1 security_framework=1 eager_bind=0"
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
  --preflight) preflight_build ;;
  --verify)
    shift
    [ "$#" -eq 1 ] || fail "--verify needs exactly one binary path"
    preflight_common
    verify "$1"
    ;;
  --help|-h)
    echo "usage: $0 --preflight | --verify <binary> | <command> [args...]"
    ;;
  '') fail "no command given (try --preflight or --help)" ;;
  *) preflight_build; run_command "$@" ;;
esac
