#!/usr/bin/env bash
# Keep the Firecracker-backed hive-cloud node alive: the host-side `dev.shadw.fc` agent. Once the Lima VM is up (the
# `dev.shadw.lima` agent owns booting it) it starts the in-VM hive-fc.service if that is not active. A no-op with a one-line
# hint until the unit has been installed once via `./scripts/shadwd.sh fc-daemon`; it never rebuilds, so every StartInterval
# is cheap.
set -uo pipefail
VM="${FC_VM:-hive}"

command -v limactl >/dev/null 2>&1 || { echo "limactl not installed — skipping fc node"; exit 0; }

st="$(limactl list --format '{{.Status}}' "$VM" 2>/dev/null)"
[ "$st" = "Running" ] || { echo "$(date '+%H:%M:%S') lima '$VM' not Running ($st) — fc node waits for it"; exit 0; }

if ! limactl shell "$VM" bash -lc 'systemctl cat hive-fc.service >/dev/null 2>&1'; then
  echo "$(date '+%H:%M:%S') hive-fc.service not installed — run: ./scripts/shadwd.sh fc-daemon (once)"
  exit 0
fi

if limactl shell "$VM" bash -lc 'systemctl is-active --quiet hive-fc.service'; then
  exit 0
fi
echo "$(date '+%H:%M:%S') starting hive-fc.service in '$VM'"
limactl shell "$VM" bash -lc 'sudo systemctl start hive-fc.service' 2>/dev/null || true
