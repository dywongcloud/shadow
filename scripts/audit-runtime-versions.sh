#!/usr/bin/env bash
# Report firecracker / crun / runsc / glibc for every node in the ansible
# inventory, so version drift is something you LOOK UP rather than discover
# during an incident.
#
# Why this exists: nothing in the platform pins these runtimes. A node
# provisioned in March and one provisioned in July silently end up on
# different versions forever. On 2026-07-31 that spread put four nodes inside
# the affected range for a firecracker guest->host RCE (CVE-2026-5747) while
# the rest of the fleet was already patched -- invisible until audited.
#
# glibc is reported alongside because it decides which nodes can share a
# binary at all (see AGENTS.md's two-glibc-groups note); a rollout that
# ignores it crash-loops the node it lands on.
#
# Usage:
#   scripts/audit-runtime-versions.sh                 # table for the whole fleet
#   scripts/audit-runtime-versions.sh --json          # machine-readable
#   SSH_KEY=~/.ssh/other.pem scripts/audit-runtime-versions.sh
set -uo pipefail

INVENTORY="${INVENTORY:-$(dirname "$0")/../ansible/inventory/hosts.ini}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/billing.pem}"
SSH_OPTS=(-o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new -o BatchMode=yes)
JSON=0
[ "${1:-}" = "--json" ] && JSON=1

if [ ! -f "$INVENTORY" ]; then
  echo "inventory not found: $INVENTORY" >&2
  exit 1
fi

# name<TAB>ip, skipping comments/group headers/vars lines.
NODES=$(awk '
  /^\[/    { in_vars = ($0 ~ /:vars\]/); next }
  /^#/     { next }
  /^[ \t]*$/ { next }
  in_vars  { next }
  {
    name = ""; ip = ""
    for (i = 1; i <= NF; i++) {
      if ($i ~ /^hive_name=/)      { split($i, a, "="); name = a[2] }
      if ($i ~ /^ansible_host=/)   { split($i, b, "="); ip   = b[2] }
    }
    if (name != "" && ip != "") print name "\t" ip
  }
' "$INVENTORY")

if [ -z "$NODES" ]; then
  echo "no nodes parsed from $INVENTORY" >&2
  exit 1
fi

probe() {
  # -n is load-bearing: without it ssh consumes the caller's stdin, which is
  # the node list the surrounding `while read` loop is iterating -- the loop
  # then silently processes only its first node.
  ssh -n -i "$SSH_KEY" "${SSH_OPTS[@]}" "root@$1" '
    fc=$(firecracker --version 2>/dev/null | head -1 | awk "{print \$2}")
    cr=$(crun --version 2>/dev/null | head -1 | awk "{print \$3}")
    rs=$(runsc --version 2>/dev/null | head -1 | awk "{print \$3}")
    gl=$(ldd --version 2>/dev/null | head -1 | awk "{print \$NF}")
    pm=$(podman --version 2>/dev/null | awk "{print \$3}")
    echo "${fc:-none}|${cr:-none}|${rs:-none}|${gl:-none}|${pm:-none}"
  ' 2>/dev/null
}

[ "$JSON" = 1 ] && echo "["
first=1
SEEN_FC=()
[ "$JSON" = 0 ] && printf "%-20s %-16s %-14s %-8s %-24s %-8s\n" \
  NODE FIRECRACKER HOST_IP GLIBC RUNSC CRUN
[ "$JSON" = 0 ] && printf "%.0s-" {1..96} && echo

while IFS=$'\t' read -r name ip; do
  [ -z "$name" ] && continue
  out=$(probe "$ip")
  if [ -z "$out" ]; then
    fc=UNREACHABLE; cr=-; rs=-; gl=-; pm=-
  else
    IFS='|' read -r fc cr rs gl pm <<< "$out"
  fi
  SEEN_FC+=("$fc")
  if [ "$JSON" = 1 ]; then
    [ $first = 0 ] && echo ","
    first=0
    printf '  {"node":"%s","ip":"%s","firecracker":"%s","crun":"%s","runsc":"%s","glibc":"%s","podman":"%s"}' \
      "$name" "$ip" "$fc" "$cr" "$rs" "$gl" "$pm"
  else
    printf "%-20s %-16s %-14s %-8s %-24s %-8s\n" "$name" "$fc" "$ip" "$gl" "$rs" "$cr"
  fi
done <<< "$NODES"

if [ "$JSON" = 1 ]; then
  echo
  echo "]"
else
  echo
  echo "Distinct firecracker versions (a spread here is the drift to investigate):"
  printf '%s\n' "${SEEN_FC[@]}" | sort | uniq -c | sort -rn | sed 's/^/  /'
fi
