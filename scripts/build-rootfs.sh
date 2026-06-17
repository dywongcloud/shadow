#!/usr/bin/env bash
# Build an ext4 rootfs for a Hive cell from an OCI image, with the cell agent
# baked in as the guest init. Run this INSIDE the Lima guest (needs root, docker,
# mkfs.ext4). The result lands in $ROOTFS_DIR/<sanitized-image>.ext4 and is what
# the Firecracker backend boots for that logical image name.
#
# Usage: sudo ./build-rootfs.sh <oci-image> <logical-name> [size-mib]
#   e.g. sudo ./build-rootfs.sh ubuntu:24.04 default 2048
set -euo pipefail

OCI_IMAGE="${1:?usage: build-rootfs.sh <oci-image> <logical-name> [size-mib]}"
LOGICAL_NAME="${2:?missing logical name}"
SIZE_MIB="${3:-2048}"

ROOTFS_DIR="${ROOTFS_DIR:-/var/lib/hive/rootfs}"
AGENT_BIN="${AGENT_BIN:-/usr/local/bin/hive-cell-agent}"

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

sudo umount "$MNT"
trap 'docker rm -f "$CID" >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT
echo ">> done: $OUT"
