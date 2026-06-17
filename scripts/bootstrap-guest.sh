#!/usr/bin/env bash
# Provision the Lima guest to run Hive's Firecracker backend.
# Run this INSIDE the Lima VM:  limactl shell hive  -> bash scripts/bootstrap-guest.sh
#
# It installs firecracker (aarch64), fetches a guest kernel, builds the Rust
# binaries (hived + hive-cell-agent) from the read-only mounted source into a
# writable target dir, and bakes a default rootfs. After this, run:
#   sudo hived --backend firecracker --warm default=2
set -euo pipefail

FC_VERSION="${FC_VERSION:-v1.13.0}"
ARCH="aarch64"
PREFIX="/usr/local/bin"
HIVE_LIB="/var/lib/hive"
# Source dir: the repo mounted read-only by Lima. Adjust if your mount differs.
SRC_DIR="${SRC_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
TARGET_DIR="${TARGET_DIR:-$HOME/hive-target}"

echo "== 0. sanity =="
[ -e /dev/kvm ] || { echo "FATAL: /dev/kvm missing — is this an M3/M4 with nestedVirtualization=true?"; exit 1; }

echo "== 1. firecracker $FC_VERSION =="
if ! command -v firecracker >/dev/null; then
  tmp="$(mktemp -d)"
  curl -fsSL "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-${ARCH}.tgz" -o "$tmp/fc.tgz"
  tar -xzf "$tmp/fc.tgz" -C "$tmp"
  sudo install -m0755 "$tmp/release-${FC_VERSION}-${ARCH}/firecracker-${FC_VERSION}-${ARCH}" "$PREFIX/firecracker"
  rm -rf "$tmp"
fi
firecracker --version | head -1

echo "== 2. guest kernel =="
sudo mkdir -p "$HIVE_LIB" "$HIVE_LIB/rootfs" /run/hive
if [ ! -f "$HIVE_LIB/vmlinux" ]; then
  # An uncompressed aarch64 kernel that boots under Firecracker. The Firecracker
  # CI publishes known-good kernels; pin/override KERNEL_URL as needed.
  KERNEL_URL="${KERNEL_URL:-https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/aarch64/vmlinux-6.1.102}"
  echo "   downloading kernel from $KERNEL_URL"
  curl -fsSL "$KERNEL_URL" -o /tmp/vmlinux
  sudo mv /tmp/vmlinux "$HIVE_LIB/vmlinux"
fi
ls -lh "$HIVE_LIB/vmlinux"

echo "== 3. rust toolchain =="
if ! command -v cargo >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

echo "== 4. build hived + hive-cell-agent (source is read-only; target=$TARGET_DIR) =="
mkdir -p "$TARGET_DIR"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release \
  --manifest-path "$SRC_DIR/Cargo.toml" \
  -p hived -p hivectl -p hive-cell-agent
sudo install -m0755 "$TARGET_DIR/release/hived"            "$PREFIX/hived"
sudo install -m0755 "$TARGET_DIR/release/hivectl"          "$PREFIX/hivectl"
sudo install -m0755 "$TARGET_DIR/release/hive-cell-agent"  "$PREFIX/hive-cell-agent"

echo "== 5. default rootfs =="
# A small image that has git + coreutils. Customize per logical image name.
sudo AGENT_BIN="$PREFIX/hive-cell-agent" ROOTFS_DIR="$HIVE_LIB/rootfs" \
  bash "$SRC_DIR/scripts/build-rootfs.sh" "buildpack-deps:24.04-curl" default 4096 || {
    echo "rootfs build failed; ensure docker works in the guest (sudo usermod -aG docker \$USER; relogin)"; exit 1; }

cat <<EOF

== done ==
Assets:
  firecracker : $PREFIX/firecracker
  kernel      : $HIVE_LIB/vmlinux
  rootfs(dir) : $HIVE_LIB/rootfs   ($(ls "$HIVE_LIB/rootfs"))
  run dir     : /run/hive

Start a real Hive (cells = Firecracker microVMs):
  sudo RUST_LOG=info,hive_controlplane=debug hived --backend firecracker --warm default=2

In another guest shell, submit a build:
  hivectl submit --image default -c 'uname -a' -c 'cat /etc/os-release' --follow
EOF
