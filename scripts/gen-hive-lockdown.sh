#!/usr/bin/env bash
# Regenerate the PEERS and PRIVATE_PEERS lines of scripts/hive-lockdown.sh from ansible/inventory/hosts.ini (the source
# of truth), so the peer roster is never hand-copied.
#
# Usage: scripts/gen-hive-lockdown.sh   (rewrites scripts/hive-lockdown.sh in place)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INVENTORY="$REPO_ROOT/ansible/inventory/hosts.ini"
TARGET="$REPO_ROOT/scripts/hive-lockdown.sh"
[ -f "$INVENTORY" ] || { echo "inventory not found: $INVENTORY" >&2; exit 1; }

live_host_values() {
  local inventory_key="$1"
  awk -v key="$inventory_key" '
    /^[[:space:]]*#/ { next }
    /ansible_host=/ {
      addr = ""; retired = 0
      for (i = 1; i <= NF; i++) {
        split($i, a, "=")
        if (a[1] == key) addr = a[2]
        if (a[1] == "hive_retired" && a[2] == "true") retired = 1
      }
      if (addr != "" && !retired) print addr
    }
  ' "$INVENTORY"
}
mapfile -t PEER_IPS < <(live_host_values ansible_host)
mapfile -t PRIVATE_IPS < <(live_host_values hive_private_ip)
[ ${#PEER_IPS[@]} -gt 0 ] || { echo "no live ansible_host entries parsed from $INVENTORY" >&2; exit 1; }

PEERS_LINE="${PEER_IPS[*]}"
PRIVATE_LINE="${PRIVATE_IPS[*]:-}"

python3 - "$TARGET" "$PEERS_LINE" "$PRIVATE_LINE" <<'PYEOF'
import re, sys
target, peers_line, private_line = sys.argv[1], sys.argv[2], sys.argv[3]
content = open(target).read()
for var, value in (("PEERS", peers_line), ("PRIVATE_PEERS", private_line)):
    content, n = re.subn(rf'^{var}="[^"]*"', f'{var}="{value}"', content, count=1, flags=re.MULTILINE)
    if n == 0:
        print(f"ERROR: {var}= line not found in {target}", file=sys.stderr)
        sys.exit(1)
open(target, "w").write(content)
print(f"wrote {len(peers_line.split())} public and {len(private_line.split())} private peers to {target}")
PYEOF
