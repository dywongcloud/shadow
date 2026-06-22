#!/usr/bin/env bash
# Ensure the Lima nested-virt VM (the REAL Firecracker microVM host) is running.
# Idempotent: a no-op if already up; creates it from scripts/fc-vm.yaml if absent.
# Invoked by the `dev.shadw.lima` LaunchAgent at login + periodically.
set -uo pipefail
VM="${FC_VM:-hive}"
DIR="$(cd "$(dirname "$0")" && pwd)"

# Clear warm-up pressure: remove stopped/leaked (e.g. "Created") host containers
# so a fresh boot doesn't inherit a saturated podman lock pool / process table.
# `container prune` only touches NON-running containers — live deployment cells
# (hive-cell-*/hive-db-*) are left alone. Runs every boot + on the lima agent's
# StartInterval, so leaks can never accumulate to the point of fork starvation.
if command -v podman >/dev/null 2>&1; then
  pruned="$(podman container prune -f 2>/dev/null | grep -c .)" || true
  [ "${pruned:-0}" != 0 ] && echo "$(date '+%H:%M:%S') pruned ${pruned} stopped/leaked containers"
fi

command -v limactl >/dev/null 2>&1 || { echo "limactl not installed"; exit 0; }

st="$(limactl list --format '{{.Status}}' "$VM" 2>/dev/null)"
case "$st" in
  Running) exit 0 ;;
  "")      exec limactl start --name="$VM" --tty=false "$DIR/fc-vm.yaml" </dev/null ;;
  *)       exec limactl start "$VM" </dev/null ;;
esac
