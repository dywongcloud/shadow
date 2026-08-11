#!/usr/bin/env bash
# Build ONE binary for ONE target and stage it into an npm platform package
# (the esbuild/swc/turbo pattern: a tiny dispatcher package with per-platform
# `optionalDependencies`, each scoped by `os`/`cpu` so npm only ever installs
# the one that matches). This script is the SINGLE source of truth for that
# staging step — both a local build (this machine, darwin only) and CI (every
# platform) call it, so the package shape can never drift between them.
#
# Usage:
#   scripts/build-npm-pkg.sh <crate> <bin-name> <npm-pkg-base> <target-triple> <npm-os> <npm-cpu>
#
# Example (what this script's own local run below invokes):
#   scripts/build-npm-pkg.sh shadw-cli shadw shadw-cli aarch64-apple-darwin darwin arm64
#   -> builds target/aarch64-apple-darwin/release/shadw
#   -> stages npm/shadw-cli-darwin-arm64/{package.json, bin/shadw}
set -euo pipefail

CRATE="${1:?crate name, e.g. shadw-cli}"
BIN="${2:?binary name, e.g. shadw}"
PKG_BASE="${3:?npm package base name, e.g. shadw-cli}"
TARGET="${4:?rust target triple, e.g. aarch64-apple-darwin}"
NPM_OS="${5:?npm os value, e.g. darwin|linux}"
NPM_CPU="${6:?npm cpu value, e.g. arm64|x64}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(grep -A2 '^\[workspace.package\]' Cargo.toml | grep '^version' | sed -E 's/version *= *"([^"]+)"/\1/')"
[ -n "$VERSION" ] || { echo "could not read workspace version from Cargo.toml" >&2; exit 1; }

PKG_NAME="${PKG_BASE}-${NPM_OS}-${NPM_CPU}"
PKG_DIR="npm/${PKG_NAME}"

echo ">> building ${CRATE} (bin ${BIN}) for ${TARGET}"
cargo build --release -p "$CRATE" --target "$TARGET"

BUILT_BIN="target/${TARGET}/release/${BIN}"
[ -f "$BUILT_BIN" ] || { echo "expected binary not found: $BUILT_BIN" >&2; exit 1; }

mkdir -p "${PKG_DIR}/bin"
cp "$BUILT_BIN" "${PKG_DIR}/bin/${BIN}"
chmod 0755 "${PKG_DIR}/bin/${BIN}"

# `os`/`cpu` are what make this SAFE as an optionalDependency: npm refuses to
# even ATTEMPT installing a package whose os/cpu does not match the current
# machine, before any network call — a Linux user's `npm install shadw` never
# touches this darwin package at all, and a failure to publish this specific
# platform package later cannot break an install on a DIFFERENT platform.
cat > "${PKG_DIR}/package.json" <<EOF
{
  "name": "${PKG_NAME}",
  "version": "${VERSION}",
  "description": "${BIN} prebuilt binary for ${NPM_OS}/${NPM_CPU} (platform package for ${PKG_BASE} — install ${PKG_BASE} instead)",
  "os": ["${NPM_OS}"],
  "cpu": ["${NPM_CPU}"],
  "bin": { "${BIN}": "bin/${BIN}" },
  "license": "MIT",
  "repository": { "type": "git", "url": "git+https://github.com/dywongcloud/hive.git" }
}
EOF

SIZE_MB=$(du -m "${PKG_DIR}/bin/${BIN}" | cut -f1)
echo ">> staged ${PKG_DIR} (${SIZE_MB} MiB binary), version ${VERSION}"
