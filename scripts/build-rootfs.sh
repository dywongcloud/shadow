#!/usr/bin/env bash
# Build an ext4 rootfs for a Hive cell from an OCI image, with the cell agent
# baked in as the guest init. Run this INSIDE the Lima guest (needs root, docker,
# mkfs.ext4). The result lands in $ROOTFS_DIR/<sanitized-image>.ext4 and is what
# the Firecracker backend boots for that logical image name.
#
# Usage: sudo ./build-rootfs.sh <oci-image> <logical-name> [size-mib]
#   e.g. sudo ./build-rootfs.sh ubuntu:24.04 default 2048
#
# WASMER (hive_core::Runtime::Wasmer): set WASMER_TARBALL=/path/to/
# wasmer-linux-amd64.tar.gz to bake the `wasmer` CLI into the guest. This is
# REQUIRED for Wasmer functions on a Firecracker node and is not optional
# plumbing: `hive-cell-agent` runs as PID1 INSIDE the microVM and execs
# `start_cmd[0]` against the GUEST PATH, so a wasmer installed on the host is
# invisible to it. Only `bin/wasmer` is staged (~200 MiB); the tarball's
# `lib/*.a` static archives are another ~560 MiB that `wasmer run` never opens.
# When staged, a `<name>.wasmer` marker file is written NEXT TO the image — that
# marker is what `hive-cloud`'s `detect_wasm_runtime` probe stats to decide
# whether this node may advertise `NodeInfo::wasm_runtime`, because mounting an
# ext4 image to look inside would need root and a loop device at every boot.
set -euo pipefail

OCI_IMAGE="${1:?usage: build-rootfs.sh <oci-image> <logical-name> [size-mib]}"
LOGICAL_NAME="${2:?missing logical name}"
SIZE_MIB="${3:-2048}"

ROOTFS_DIR="${ROOTFS_DIR:-/var/lib/hive/rootfs}"
AGENT_BIN="${AGENT_BIN:-/usr/local/bin/hive-cell-agent}"
WASMER_TARBALL="${WASMER_TARBALL:-}"

# Sanitize the logical name the same way the Rust backend does (`/`,`:` -> `_`).
SAFE_NAME="$(printf '%s' "$LOGICAL_NAME" | tr -c 'A-Za-z0-9.-' '_')"
OUT="${ROOTFS_DIR}/${SAFE_NAME}.ext4"

[ -x "$AGENT_BIN" ] || { echo "missing agent binary at $AGENT_BIN (run bootstrap-guest.sh first)"; exit 1; }
mkdir -p "$ROOTFS_DIR"

echo ">> exporting $OCI_IMAGE filesystem"
CID="$(docker create "$OCI_IMAGE" /bin/true)"
trap 'docker rm -f "$CID" >/dev/null 2>&1 || true' EXIT

WORK="$(mktemp -d)"
MNT="$WORK/mnt"
mkdir -p "$MNT"

echo ">> creating ${SIZE_MIB}MiB ext4 at $OUT"
dd if=/dev/zero of="$OUT" bs=1M count="$SIZE_MIB" status=none
mkfs.ext4 -F -q "$OUT"
sudo mount -o loop "$OUT" "$MNT"
trap 'sudo umount "$MNT" 2>/dev/null || true; docker rm -f "$CID" >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT

echo ">> unpacking container fs into image"
docker export "$CID" | sudo tar -x -C "$MNT"

echo ">> installing cell agent as /sbin/hive-cell-agent and build dir"
sudo install -D -m0755 "$AGENT_BIN" "$MNT/sbin/hive-cell-agent"
sudo mkdir -p "$MNT/build" "$MNT/root" "$MNT/proc" "$MNT/sys" "$MNT/dev" "$MNT/tmp"

# Make sure git + a shell exist (most base images have sh; add git if missing).
if [ ! -e "$MNT/usr/bin/git" ] && [ ! -e "$MNT/bin/git" ]; then
  echo "   note: '$OCI_IMAGE' has no git; clones will fail. Bake git into your image if needed."
fi

# Wasmer CLI into the GUEST (see the header note for why the host copy cannot
# serve this purpose). /usr/local/bin is on the exact PATH the agent sets.
MARKER="${ROOTFS_DIR}/${SAFE_NAME}.wasmer"
sudo rm -f "$MARKER"
if [ -n "$WASMER_TARBALL" ]; then
  [ -f "$WASMER_TARBALL" ] || { echo "WASMER_TARBALL=$WASMER_TARBALL not found"; exit 1; }
  echo ">> baking wasmer CLI into the guest at /usr/local/bin/wasmer"
  WTMP="$WORK/wasmer"
  mkdir -p "$WTMP"
  # Extract ONLY the CLI: the lib/*.a archives are ~560 MiB of build-time
  # artifacts `wasmer run` never opens, and this image has a fixed size budget.
  tar -xzf "$WASMER_TARBALL" -C "$WTMP" bin/wasmer
  sudo install -D -m0755 "$WTMP/bin/wasmer" "$MNT/usr/local/bin/wasmer"
  # Fail loudly rather than shipping an image that silently cannot run wasm.
  [ -x "$MNT/usr/local/bin/wasmer" ] || { echo "wasmer staging failed"; exit 1; }
  WVER="$("$WTMP/bin/wasmer" --version 2>/dev/null || echo unknown)"
  echo "   staged: $WVER"
fi

sudo umount "$MNT"
trap 'docker rm -f "$CID" >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT
# Write the capability marker only AFTER a clean unmount, so a failed build
# never leaves a node advertising a capability its image does not have.
if [ -n "$WASMER_TARBALL" ]; then
  printf '%s\n' "$WVER" | sudo tee "$MARKER" >/dev/null
  echo ">> wrote capability marker: $MARKER"
fi
echo ">> done: $OUT"
