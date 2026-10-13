#!/usr/bin/env bash
# Run the REAL Firecracker microVM tenancy test inside the Lima nested-virt VM (Firecracker needs Linux + /dev/kvm; on a Mac
# that is a Lima `vz` VM with nested virtualization, M3+/macOS 15+). It builds against a VM-local target dir so it never
# clobbers the host build, and runs as root for /dev/kvm and /var/lib/hive. The VM needs firecracker, a kernel and a default
# rootfs installed: scripts/bootstrap-guest.sh does that.
#
# Usage:   ./scripts/fc-test.sh
# Env:     FC_VM=<lima-instance>   (default: hive)
set -uo pipefail

VM="${FC_VM:-hive}"
HOST_REPO_PATH="$(cd "$(dirname "$0")/.." && pwd)"

command -v limactl >/dev/null 2>&1 || { echo "ERROR: limactl not found (brew install lima)"; exit 1; }

status="$(limactl list --format '{{.Status}}' "$VM" 2>/dev/null)"
if [ "$status" != "Running" ]; then
  echo "ERROR: Lima VM '$VM' is not running (status: ${status:-absent})."
  echo "  Start it:   limactl start $VM"
  echo "  Or create:  limactl start --name=$VM scripts/fc-vm.yaml   (then install firecracker + kernel/rootfs)"
  exit 1
fi

echo "==> Firecracker tenancy test in VM '$VM' ($HOST_REPO_PATH)"
limactl shell "$VM" bash -lc "
  set -e
  cd '$HOST_REPO_PATH'
  export CARGO_TARGET_DIR=\$HOME/fc-target
  echo '--> building (VM-local target dir)…'
  cargo test -p hive-backend --test firecracker_tenant --no-run 2>&1 | tail -2
  BIN=\$(ls -t \$HOME/fc-target/debug/deps/firecracker_tenant-* 2>/dev/null | grep -v '\.d\$' | head -1)
  [ -n \"\$BIN\" ] || { echo 'ERROR: test binary not found after build'; exit 1; }
  echo '--> booting real microVMs (sudo for /dev/kvm)…'
  sudo -E \"\$BIN\" --nocapture --test-threads=1
"
