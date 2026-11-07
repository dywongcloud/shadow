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
# Lockdown layers. Each node can carry TWO enforcement layers, and both hook
# INPUT, so a packet must pass BOTH (the effective policy is their
# INTERSECTION):
#   inet    `table inet hive_lockdown` -- what scripts/hive-lockdown.sh builds
#   legacy  `chain HIVE_LOCKDOWN` inside the vendor `ip filter` table -- the
#           iptables-nft era chain older hosts still carry. Once anything adds a
#           native nft rule to it, `iptables -S` refuses the whole table
#           ("incompatible"), so it is read through `nft`, never `iptables`.
# Each layer's peer set is compared, as a SET, with the live inventory roster:
# a missing peer is a node the lockdown would drop, an extra one is an address
# that no longer belongs to the fleet (a released IP is re-issued to strangers).
# Both layers present at once is flagged: after a roll the stale legacy chain
# keeps enforcing its OLD roster beside the new table.
#
# Fail2ban bans land in table inet f2b-table. The F2B_TABLE column reports
# whether that table exists and how many ban/set rules it carries, so a jail
# that is registered but never bans is visible after an apply. Only the COUNT
# is printed -- never the banned addresses or any inventory content.
#
# Usage:
#   scripts/audit-public-listeners.sh            # table for the whole fleet
#   SSH_KEY=~/.ssh/other.pem scripts/audit-public-listeners.sh
#   ONLY=fc-sanjose-7 scripts/audit-public-listeners.sh   # just these hive_names
#                                    # (comma-separated, exact, must be live
#                                    # inventory hosts); the roster is still
#                                    # compared with the WHOLE live roster
#   INCLUDE_RETIRED=1 scripts/audit-public-listeners.sh   # also probe
#                                    # hive_retired=true hosts (normally skipped:
#                                    # the instance is gone); they never join the
#                                    # expected roster, matching the generator
#   SSH_KEY=~/.ssh/id_ed25519 scripts/audit-public-listeners.sh  # fc-orlando
#   ALLOW_SKIPPED=1 ...              # accept hosts this script cannot audit
# Exit status: 0 clean, 1 on a finding (foreign listener, missing/extra peer,
# two layers, no lockdown, unreachable or unaudited host), 2 when nothing was
# audited (bad ONLY, empty selection). A host whose ansible_user is not root is
# probed with sudo -n timeout 60 sh -s (fc-orlando: sysadmin), NEVER skipped
# up front; it is reported SKIPPED only if that probe fails, and still counts
# as a finding unless ALLOW_SKIPPED=1.
set -uo pipefail

INVENTORY="${INVENTORY:-$(dirname "$0")/../ansible/inventory/hosts.ini}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS=(-o ConnectTimeout=8 -o ServerAliveInterval=5 -o ServerAliveCountMax=3 -o StrictHostKeyChecking=accept-new -o BatchMode=yes)

if [ ! -f "$INVENTORY" ]; then
  echo "inventory not found: $INVENTORY" >&2
  exit 1
fi

# name<TAB>ip<TAB>ssh-user<TAB>retired(0|1), skipping comments/group
# headers/vars lines (same parse as scripts/audit-runtime-versions.sh).
ROWS=$(awk '
  /^\[/    { in_vars = ($0 ~ /:vars\]/); next }
  /^#/     { next }
  /^[ \t]*$/ { next }
  in_vars  { next }
  {
    name = ""; ip = ""; user = "root"; retired = 0
    for (i = 1; i <= NF; i++) {
      if ($i ~ /^hive_name=/)      { split($i, a, "="); name = a[2] }
      if ($i ~ /^ansible_host=/)   { split($i, b, "="); ip   = b[2] }
      if ($i ~ /^ansible_user=/)   { split($i, c, "="); user = c[2]; gsub(/"/, "", user) }
      if ($i == "hive_retired=true") retired = 1
    }
    if (name != "" && ip != "") print name "\t" ip "\t" user "\t" retired
  }
' "$INVENTORY")

# The expected roster is the LIVE inventory (retired hosts excluded) -- the same
# rule scripts/gen-hive-lockdown.sh applies -- whatever INCLUDE_RETIRED probes.
ROSTER=$(printf '%s\n' "$ROWS" | awk -F'\t' '$4 == 0 { print $2 }' | sort -u)
NODES=$(printf '%s\n' "$ROWS" | awk -F'\t' -v inc="${INCLUDE_RETIRED:-}" 'inc == "1" || $4 == 0')

if [ -z "$NODES" ] || [ -z "$ROSTER" ]; then
  echo "no live nodes parsed from $INVENTORY" >&2
  exit 1
fi
expected_peers=$(printf '%s\n' "$ROSTER" | wc -l | tr -d ' ')

# ONLY must name probed inventory hosts exactly (comma-separated, spaces
# ignored). A typo, an inventory alias or a retired host used to audit nothing
# and still print "clean".
ONLY="${ONLY:-}"; ONLY="${ONLY// /}"
if [ -n "$ONLY" ]; then
  IFS=',' read -ra only_names <<< "$ONLY"
  for only_name in "${only_names[@]}"; do
    printf '%s\n' "$NODES" | cut -f1 | grep -qxF -- "$only_name" \
      || { echo "ONLY names an unknown or retired host: $only_name" >&2; exit 2; }
  done
fi

probe() {
  # The remote script travels on stdin (`sh -s`), so it needs no quoting of its
  # own. Do NOT add `-n`: it would swallow that stdin. A host with a non-root
  # ansible_user (fc-orlando: sysadmin) is reached as THAT user through
  # passwordless sudo -n -- the same pattern as scripts/audit-secret-keys.sh --
  # so a provisioned-but-non-root node is audited, never skipped up front.
  local ip="$1"
  local user="${2:-root}"
  local remote_cmd='timeout 60 sh -s'
  if [ "$user" != root ]; then
    remote_cmd='sudo -n timeout 60 sh -s'
  fi
  ssh -i "$SSH_KEY" "${SSH_OPTS[@]}" "$user@$ip" "$remote_cmd" 2>/dev/null <<'REMOTE'
# Wildcard listeners (0.0.0.0 / [::] / *), minus the platform daemons.
foreign=$(ss -tlnpH 2>/dev/null \
  | awk '$4 ~ /^(0\.0\.0\.0|\[::\]|\*):/' \
  | grep -vE '"(hive-cloud|sshd|iroh-relay|rpc-server|llama-server)"' \
  | grep -vE ':9090 .*"systemd"' \
  | sed -E 's/^LISTEN +[0-9]+ +[0-9]+ +([^ ]+) +[^ ]+ +users:\(\("([^"]+)",pid=([0-9]+).*/\1 \2 pid=\3/' \
  | tr '\n' ';')
peers_of() { sort -u | tr '\n' ',' | sed 's/,$//'; }
layers=""
# legacy: the vendor `ip filter` chain read through nft (an iptables-nft chain
# that `iptables -S` refuses to list is still found); plain iptables only on a
# host with no such chain.
if nft list chain ip filter HIVE_LOCKDOWN >/dev/null 2>&1; then
  ips=$(nft list chain ip filter HIVE_LOCKDOWN | sed -n 's/^[[:space:]]*ip saddr \([0-9.]*\) \(counter packets [0-9]* bytes [0-9]* \)\{0,1\}\(return\|accept\)[[:space:]]*$/\1/p' | peers_of)
  layers="$layers legacy=$ips"
elif command -v iptables >/dev/null 2>&1 && iptables -S HIVE_LOCKDOWN >/dev/null 2>&1; then
  ips=$(iptables -S HIVE_LOCKDOWN | sed -n 's/^-A HIVE_LOCKDOWN -s \([0-9.]*\)\(\/32\)\{0,1\} -j RETURN$/\1/p' | peers_of)
  layers="$layers legacy=$ips"
fi
if nft list table inet hive_lockdown >/dev/null 2>&1; then
  ips=$(nft list table inet hive_lockdown | tr '\n' ' ' | grep -o 'ip saddr {[^}]*} accept' | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | peers_of)
  layers="$layers inet=$ips"
fi
# Fail2ban's own ban table. Report presence + a rule count only: never echo
# the banned addresses (and never the inventory) back to the audit output.
f2b="absent"
if nft list table inet f2b-table >/dev/null 2>&1; then
  f2b_rules=$(nft list table inet f2b-table 2>/dev/null | grep -cE '(^|[[:space:]])(drop|reject|elements)([[:space:]]|$|=)' || true)
  f2b="present:${f2b_rules:-0}"
fi
echo "${layers# }|${foreign}|${f2b}"
REMOTE
}

# $1 = comma-joined peers one layer enforces -> "missing|extra" (both comma-joined)
roster_diff() {
  local have missing extra
  have=$(printf '%s' "$1" | tr ',' '\n' | grep -v '^$' | sort -u)
  missing=$(comm -23 <(printf '%s\n' "$ROSTER") <(printf '%s\n' "$have") | grep -v '^$' | tr '\n' ',' | sed 's/,$//')
  extra=$(comm -13 <(printf '%s\n' "$ROSTER") <(printf '%s\n' "$have") | grep -v '^$' | tr '\n' ',' | sed 's/,$//')
  printf '%s|%s' "$missing" "$extra"
}

printf "%-20s %-16s %-32s %-14s %s\n" NODE HOST_IP LOCKDOWN F2B_TABLE FOREIGN_WILDCARD_LISTENERS
printf "%.0s-" {1..124} && echo

bad=0; audited=0; skipped=0
while IFS=$'\t' read -r name ip user retired; do
  [ -z "$name" ] && continue
  if [ -n "$ONLY" ] && [[ ",${ONLY}," != *",${name},"* ]]; then continue; fi
  # Every live node is probed, including a host with a non-root ansible_user
  # (fc-orlando: sysadmin + passwordless sudo -n). A node that is never probed
  # can never count toward a clean verdict, so there is no pre-emptive skip.
  out=$(probe "$ip" "${user:-root}")
  if [ -z "$out" ]; then
    if [ "${user:-root}" != root ]; then
      printf "%-20s %-16s %-32s %-14s %s\n" "$name" "$ip" SKIPPED "-" "ansible_user=$user needs passwordless sudo -n -- audit this host by hand"
      skipped=$((skipped + 1))
      continue
    fi
    printf "%-20s %-16s %-32s %-14s %s\n" "$name" "$ip" UNREACHABLE "-" "ssh failed -- try: ssh -i $SSH_KEY root@$ip true"
    bad=1
    continue
  fi
  audited=$((audited + 1))
  layers="${out%%|*}"; rest="${out#*|}"; foreign="${rest%%|*}"; f2b="${rest#*|}"
  tag=""; detail=""
  if [ -z "$layers" ]; then
    tag="NONE!"; bad=1
  else
    for tok in $layers; do
      lname="${tok%%=*}"; csv="${tok#*=}"
      diff=$(roster_diff "$csv"); missing="${diff%%|*}"; extra="${diff#*|}"
      have=$(printf '%s' "$csv" | tr ',' '\n' | grep -c .)
      if [ -z "$missing" ] && [ -z "$extra" ]; then
        piece="$lname:$have/$expected_peers"
      else
        piece="$lname:$have/$expected_peers!"; bad=1
        detail="$detail ${lname}: missing=${missing:--} extra=${extra:--};"
      fi
      tag="$tag${tag:+ }$piece"
    done
    case "$layers" in
      *legacy=*inet=*|*inet=*legacy=*)
        tag="$tag BOTH!"; bad=1
        detail="$detail both layers hook INPUT, so the effective policy is their intersection -- retire the legacy chain (scripts/hive-lockdown.sh does when rolled);" ;;
    esac
  fi
  if [ -n "$foreign" ]; then
    bad=1
    printf "%-20s %-16s %-32s %-14s %s\n" "$name" "$ip" "$tag" "$f2b" "$foreign"
  else
    printf "%-20s %-16s %-32s %-14s %s\n" "$name" "$ip" "$tag" "$f2b" "-"
  fi
  [ -n "$detail" ] && printf "    %s\n" "${detail# }"
done <<< "$NODES"

echo
echo "audited $audited host(s), skipped $skipped"
if [ "$audited" = 0 ]; then
  echo "nothing was audited -- every selected host was skipped or filtered out." >&2
  exit 2
fi
if [ "$skipped" -gt 0 ] && [ "${ALLOW_SKIPPED:-}" != 1 ]; then
  echo "$skipped live host(s) were SKIPPED and not audited (ALLOW_SKIPPED=1 accepts that)."
  bad=1
fi
if [ "$bad" = 1 ]; then
  echo "FOREIGN LISTENER, MISSING/EXTRA PEER, TWO LAYERS, NO LOCKDOWN, UNAUDITED OR UNREACHABLE NODE ABOVE -- investigate before the next roll."
  echo "(roster: regenerate with scripts/gen-hive-lockdown.sh, roll with 'ansible-playbook playbooks/site.yml --tags lockdown')"
  exit 1
fi
echo "clean: no foreign wildcard listeners; every audited node enforces exactly the ${expected_peers}-peer live roster in one layer."
