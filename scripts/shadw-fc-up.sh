#!/usr/bin/env bash
# Keep the REAL Firecracker-backed hive-cloud node alive. This is the host-side
# "fifth agent" (dev.shadw.fc): it ensures the Lima nested-virt VM is up and that
# the in-VM `hive-fc.service` (a hive-cloud node using the Firecracker CellBackend
# on /dev/kvm) is running. The node listens INSIDE the VM on :8796/:8797, which
# Lima forwards to host 127.0.0.1:8796/8797 — distinct from the mock host nodes
# (:8786/:8787, :8788/:8789) so they coexist with no port clash.
#
# Idempotent + fully defensive: a no-op (with a one-line hint) until the unit has
# been installed once via `./scripts/shadwd.sh fc-daemon`. It NEVER rebuilds here
# (that's heavy) — it only starts an already-installed service — so running every
# StartInterval is cheap and can't storm.
set -uo pipefail
VM="${FC_VM:-hive}"

command -v limactl >/dev/null 2>&1 || { echo "limactl not installed — skipping fc node"; exit 0; }

# VM must be up first (the dev.shadw.lima agent owns booting it; don't race it).
st="$(limactl list --format '{{.Status}}' "$VM" 2>/dev/null)"
[ "$st" = "Running" ] || { echo "$(date '+%H:%M:%S') lima '$VM' not Running ($st) — fc node waits for it"; exit 0; }

# Is the systemd unit installed in the VM?
if ! limactl shell "$VM" bash -lc 'systemctl cat hive-fc.service >/dev/null 2>&1'; then
  echo "$(date '+%H:%M:%S') hive-fc.service not installed — run: ./scripts/shadwd.sh fc-daemon (once)"
  exit 0
fi

# Installed: start it if it isn't already active (cheap, no rebuild).
if limactl shell "$VM" bash -lc 'systemctl is-active --quiet hive-fc.service'; then
  exit 0
fi
echo "$(date '+%H:%M:%S') starting hive-fc.service in '$VM'"
limactl shell "$VM" bash -lc 'sudo systemctl start hive-fc.service' 2>/dev/null || true
