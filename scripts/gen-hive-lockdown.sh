#!/usr/bin/env bash
# Regenerates scripts/hive-lockdown.sh's PEERS line from
# ansible/inventory/hosts.ini -- SOURCE OF TRUTH, matching the same
# awk-derivation style scripts/deploy-ui-fleet.sh already uses for its host
# list, so the peer roster is never hand-copied (and never drifts) again.
#
# Usage: scripts/gen-hive-lockdown.sh   (rewrites scripts/hive-lockdown.sh in place)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INVENTORY="$REPO_ROOT/ansible/inventory/hosts.ini"
TARGET="$REPO_ROOT/scripts/hive-lockdown.sh"
[ -f "$INVENTORY" ] || { echo "inventory not found: $INVENTORY" >&2; exit 1; }

mapfile -t PEER_IPS < <(
  awk '
    /ansible_host=/ {
      for (i=1; i<=NF; i++) {
        if ($i ~ /^ansible_host=/) { split($i, a, "="); print a[2] }
      }
    }
  ' "$INVENTORY"
)
[ ${#PEER_IPS[@]} -gt 0 ] || { echo "no ansible_host entries parsed from $INVENTORY" >&2; exit 1; }

PEERS_LINE="${PEER_IPS[*]}"

python3 - "$TARGET" "$PEERS_LINE" <<'PYEOF'
import re, sys
target, peers_line = sys.argv[1], sys.argv[2]
content = open(target).read()
new_content, n = re.subn(
    r'^PEERS="[^"]*"',
    f'PEERS="{peers_line}"',
    content,
    count=1,
    flags=re.MULTILINE,
)
if n == 0:
    print("WARNING: PEERS= line not found/replaced", file=sys.stderr)
    sys.exit(1)
open(target, "w").write(new_content)
print(f"wrote {len(peers_line.split())} peers to {target}")
PYEOF
