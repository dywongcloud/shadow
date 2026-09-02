#!/usr/bin/env bash
# Report every wildcard-bound TCP listener on every fleet node that is NOT one
# of the platform's own daemons, plus the hive-lockdown roster each node is
# actually enforcing -- so an exposed port is something you LOOK UP rather
# than learn from a cloud-provider vulnerability ticket.
#
# Why this exists: on 2026-08-26 two `python3 -m http.server --bind 0.0.0.0`
# processes (an ad-hoc binary hand-off between nodes) were left running on
# fc-virginia on ports 28126/28127 -- inside the 20000-29999 range the Tencent
# security group opens to the whole internet for published container ports.
# They served a directory listing to the world for a week until Tencent's
# scanner filed a ticket against the account. Nothing in the platform noticed
# because nothing looked. The same pass found the lockdown peer roster at 13
# of 22 hosts on every node.
#
# What "platform's own" means here (everything else is reported):
#   hive-cloud        every listener it opens (admin 8786/8787, raw proxy
#                     20000-29999, DNS 53, discovery 3350, ...)
#   sshd              22
#   iroh-relay        3340 / 3343 (embedded relay, fleet-wide)
#   rpc-server        50052   (llama.cpp ggml RPC, GPU nodes; lockdown-dropped)
#   llama-server      50100-50999 (managed inference; lockdown-dropped)
#   systemd :9090     cockpit.socket on Rocky images; lockdown-dropped
# A `next-server`/`node`/`python3`/... listener on 0.0.0.0 is exactly what this
# script is for. Tenant containers publish on 127.0.0.1 only (see
# `container_cli` -- every backend emits loopback publishes), so a container
# never appears here; a host-spawned function that binds wildcard does.
#
# Usage:
#   scripts/audit-public-listeners.sh            # table for the whole fleet
#   SSH_KEY=~/.ssh/other.pem scripts/audit-public-listeners.sh
# Exit status is 1 when any node reports a foreign listener, so it can gate a
# rollout or run from cron.
set -uo pipefail

INVENTORY="${INVENTORY:-$(dirname "$0")/../ansible/inventory/hosts.ini}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/billing.pem}"
SSH_OPTS=(-o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new -o BatchMode=yes)

if [ ! -f "$INVENTORY" ]; then
  echo "inventory not found: $INVENTORY" >&2
  exit 1
fi

# name<TAB>ip, skipping comments/group headers/vars lines (same parse as
# scripts/audit-runtime-versions.sh).
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
  # the node list the surrounding `while read` loop is iterating.
  ssh -n -i "$SSH_KEY" "${SSH_OPTS[@]}" "root@$1" '
    # Wildcard listeners (0.0.0.0 / [::] / *), minus the platform daemons.
    foreign=$(ss -tlnpH 2>/dev/null \
      | awk "\$4 ~ /^(0\\.0\\.0\\.0|\\[::\\]|\\*):/" \
      | grep -vE "\"(hive-cloud|sshd|iroh-relay|rpc-server|llama-server)\"" \
      | grep -vE ":9090 .*\"systemd\"" \
      | sed -E "s/^LISTEN +[0-9]+ +[0-9]+ +([^ ]+) +[^ ]+ +users:\(\(\"([^\"]+)\",pid=([0-9]+).*/\1 \2 pid=\3/" \
      | tr "\n" ";")
    # Which lockdown branch is live and how many peers it exempts.
    if command -v iptables >/dev/null 2>&1 && iptables -S HIVE_LOCKDOWN >/dev/null 2>&1; then
      lock="iptables:$(iptables -S HIVE_LOCKDOWN | grep -c -- "-s ")"
    elif nft list table inet hive_lockdown >/dev/null 2>&1; then
      lock="nft:$(nft list table inet hive_lockdown | grep -o "ip saddr {[^}]*}" | tr "," "\n" | wc -l | tr -d " ")"
    else
      lock="NONE"
    fi
    echo "${lock}|${foreign}"
  ' 2>/dev/null
}

expected_peers=$(printf '%s\n' "$NODES" | wc -l | tr -d ' ')
printf "%-20s %-16s %-16s %s\n" NODE HOST_IP LOCKDOWN FOREIGN_WILDCARD_LISTENERS
printf "%.0s-" {1..100} && echo

bad=0
while IFS=$'\t' read -r name ip; do
  [ -z "$name" ] && continue
  out=$(probe "$ip")
  if [ -z "$out" ]; then
    printf "%-20s %-16s %-16s %s\n" "$name" "$ip" UNREACHABLE -
    bad=1
    continue
  fi
  lock="${out%%|*}"; foreign="${out#*|}"
  peers="${lock#*:}"
  tag="$lock"
  if [ "$lock" = "NONE" ]; then
    tag="NONE!"; bad=1
  elif [ "$peers" != "$expected_peers" ]; then
    tag="$lock/$expected_peers!"; bad=1
  fi
  if [ -n "$foreign" ]; then
    bad=1
    printf "%-20s %-16s %-16s %s\n" "$name" "$ip" "$tag" "$foreign"
  else
    printf "%-20s %-16s %-16s %s\n" "$name" "$ip" "$tag" "-"
  fi
done <<< "$NODES"

echo
if [ "$bad" = 1 ]; then
  echo "FOREIGN LISTENER, MISSING LOCKDOWN, STALE ROSTER OR UNREACHABLE NODE ABOVE -- investigate before the next roll."
  echo "(roster: regenerate with scripts/gen-hive-lockdown.sh, roll with 'ansible-playbook playbooks/site.yml --tags lockdown')"
  exit 1
fi
echo "clean: no foreign wildcard listeners; every node enforces the ${expected_peers}-peer lockdown roster."
