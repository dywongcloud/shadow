#!/usr/bin/env bash
# Report, for every fleet node, whether the package registry its host-lane
# builds will actually use is reachable -- so a dead resolver or mirror is
# something you LOOK UP rather than learn from a tenant's failed build.
#
# Why this exists: on 2026-10-13 every uncached `npm ci` / `npm install` on
# fc-sanjose failed (`ENOTFOUND mirrors.tencentyun.com`, or npm 10's `Exit
# handler never called!`). `/etc/resolv.conf` had been overwritten four days
# earlier (`nameserver 1.1.1.1` + `options use-vc`, a probe-session leftover),
# and the root user's `~/.npmrc` points npm at the provider-internal mirror
# `https://mirrors.tencentyun.com/npm`, which only the provider's resolvers
# (183.60.83.19 / 183.60.82.98) can answer. The failure was invisible for four
# days because installs whose whole dependency set sat warm in the npm cache
# kept working, and nothing on the node ever tested the registry.
#
# Per node this runs, as root, exactly what a build does (HOME=/root):
#   registry   `npm config get registry` -- the registry npm will use
#   DNS        getent hosts <registry host>
#   TCP        connect to <registry host>:<port>
#   HTTP       GET <registry>/typescript (200 = the registry serves packuments)
#   PUBLIC     GET https://registry.npmjs.org/typescript (the fallback target)
#   RESOLVERS  the nameservers in /etc/resolv.conf, so a hand edit is visible
#
# A node is flagged when its configured registry is not usable. The hive-cloud
# build guard (`build_registry.rs`) papers over a dark PUBLIC-MIRROR registry
# per build; this script is how the operator finds the cause.
#
# Usage:
#   scripts/audit-build-registry.sh
#   ONLY=fc-sanjose,fc-virginia scripts/audit-build-registry.sh   # hive_names
#   SSH_KEY=~/.ssh/other.pem scripts/audit-build-registry.sh
# Exit status is 1 when any node's registry is dark or the node is unreachable.
set -uo pipefail

INVENTORY="${INVENTORY:-$(dirname "$0")/../ansible/inventory/hosts.ini}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/billing.pem}"
SSH_OPTS=(-o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new -o BatchMode=yes)

if [ ! -f "$INVENTORY" ]; then
  echo "inventory not found: $INVENTORY" >&2
  exit 1
fi

# name<TAB>ip<TAB>ssh-user, skipping comments/group headers/vars lines and
# every `hive_retired=true` host (same parse and same rule as
# scripts/audit-public-listeners.sh; INCLUDE_RETIRED=1 puts them back).
NODES=$(awk '
  /^\[/    { in_vars = ($0 ~ /:vars\]/); next }
  /^#/     { next }
  /^[ \t]*$/ { next }
  in_vars  { next }
  {
    name = ""; ip = ""; user = "root"; retired = 0
    for (i = 1; i <= NF; i++) {
      if ($i ~ /^hive_name=/)      { split($i, a, "="); name = a[2] }
      if ($i ~ /^ansible_host=/)   { split($i, b, "="); ip   = b[2] }
      if ($i ~ /^ansible_user=/)   { split($i, c, "="); user = c[2] }
      if ($i == "hive_retired=true") retired = 1
    }
    if (name != "" && ip != "" && (!retired || ENVIRON["INCLUDE_RETIRED"] == "1")) print name "\t" ip "\t" user
  }
' "$INVENTORY")

if [ -z "$NODES" ]; then
  echo "no nodes parsed from $INVENTORY" >&2
  exit 1
fi

# ONLY must name live inventory hosts exactly (comma-separated, spaces ignored):
# a typo, an alias or a retired host used to audit nothing and still print "clean".
ONLY="${ONLY:-}"; ONLY="${ONLY// /}"
if [ -n "$ONLY" ]; then
  IFS=',' read -ra only_names <<< "$ONLY"
  for only_name in "${only_names[@]}"; do
    printf '%s\n' "$NODES" | cut -f1 | grep -qxF -- "$only_name" \
      || { echo "ONLY names an unknown or retired host: $only_name" >&2; exit 2; }
  done
fi

probe() {
  # -n is load-bearing: without it ssh consumes the caller's stdin, which is
  # the node list the surrounding `while read` loop is iterating.
  ssh -n -i "$SSH_KEY" "${SSH_OPTS[@]}" "root@$1" '
    reg=$(HOME=/root npm config get registry 2>/dev/null | head -1)
    [ -n "$reg" ] || reg=https://registry.npmjs.org/
    authority=${reg#*://}; authority=${authority%%/*}
    host=${authority%%:*}
    port=443; case "$reg" in http://*) port=80 ;; esac
    case "$authority" in *:*) port=${authority##*:} ;; esac
    dns=ok;  getent hosts "$host" >/dev/null 2>&1 || dns=FAIL
    tcp=ok;  timeout 4 bash -c "echo > /dev/tcp/$host/$port" 2>/dev/null || tcp=FAIL
    http=$(curl -s -m 8 -o /dev/null -w "%{http_code}" "${reg%/}/typescript" 2>/dev/null); http=${http:-000}
    pub=$(curl -s -m 8 -o /dev/null -w "%{http_code}" https://registry.npmjs.org/typescript 2>/dev/null); pub=${pub:-000}
    ns=$(awk "/^nameserver/ { printf \"%s,\", \$2 }" /etc/resolv.conf 2>/dev/null | sed "s/,\$//")
    echo "${host}|${dns}|${tcp}|${http}|${pub}|${ns}"
  ' 2>/dev/null
}

printf "%-18s %-16s %-26s %-5s %-5s %-5s %-6s %s\n" NODE HOST_IP REGISTRY DNS TCP HTTP PUBLIC RESOLVERS
printf "%.0s-" {1..110} && echo

bad=0; audited=0; skipped=0
while IFS=$'\t' read -r name ip user; do
  [ -z "$name" ] && continue
  if [ -n "${ONLY:-}" ] && [[ ",${ONLY}," != *",${name},"* ]]; then continue; fi
  if [ "${user:-root}" != root ]; then
    printf "%-18s %-16s %s\n" "$name" "$ip" "SKIPPED (ansible_user=$user is not root -- audit this host by hand)"
    skipped=$((skipped + 1))
    continue
  fi
  audited=$((audited + 1))
  out=$(probe "$ip")
  if [ -z "$out" ]; then
    printf "%-18s %-16s %s\n" "$name" "$ip" UNREACHABLE
    bad=1
    continue
  fi
  IFS='|' read -r host dns tcp http pub ns <<< "$out"
  flag=""
  if [ "$dns" != ok ] || [ "$tcp" != ok ] || [ "$http" != 200 ]; then
    flag="  <-- REGISTRY DARK"
    [ "$pub" = 200 ] || flag="  <-- REGISTRY DARK, PUBLIC ALSO DARK"
    bad=1
  fi
  printf "%-18s %-16s %-26s %-5s %-5s %-5s %-6s %s%s\n" "$name" "$ip" "$host" "$dns" "$tcp" "$http" "$pub" "$ns" "$flag"
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
  echo "DARK REGISTRY, UNAUDITED OR UNREACHABLE NODE ABOVE -- builds that need an uncached package will fail there."
  echo "(first suspect: /etc/resolv.conf -- 'nmcli general reload dns-rc' regenerates it on NetworkManager hosts)"
  exit 1
fi
echo "clean: every audited node resolves and reaches the registry its builds use."
