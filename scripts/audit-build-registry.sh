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
#   REGISTRY   `npm config get registry` -- the registry npm will use
#              (NO-NPM when the node has no npm at all: every install fails)
#   DNS        getent hosts <registry host> (skipped for an IP literal)
#   TCP        connect to <registry host>:<port>
#   HTTP       HEAD <registry>/typescript -- 2xx/3xx/401/403 all mean a server
#              answers (an authenticated or redirecting registry is alive); a
#              HEAD, never a GET: the Tencent mirror ignores Range and a GET
#              downloads the 21 MB packument.
#   PUBLIC     HEAD https://registry.npmjs.org/typescript (the fallback target
#              `build_registry.rs` switches a build to when REGISTRY is dark)
#   RESOLVERS  the nameservers in /etc/resolv.conf, so a hand edit is visible
#
# A node is flagged when its configured registry is not usable. The hive-cloud
# build guard (`build_registry.rs`) papers over a dark PUBLIC-MIRROR registry
# per build; this script is how the operator finds the cause.
#
# Usage:
#   scripts/audit-build-registry.sh
#   ONLY=fc-sanjose,fc-virginia scripts/audit-build-registry.sh   # hive_names,
#                                    # exact, comma-separated, spaces ignored
#   INCLUDE_RETIRED=1 ...            # also probe hive_retired=true hosts
#   ALLOW_SKIPPED=1 ...              # accept hosts this script cannot audit
#   SSH_KEY=~/.ssh/other.pem scripts/audit-build-registry.sh
# Exit status: 0 clean, 1 when any node's registry is dark, has no npm, is
# unreachable or was skipped (unless ALLOW_SKIPPED=1), 2 when nothing was
# audited (bad ONLY, empty selection).
set -uo pipefail

INVENTORY="${INVENTORY:-$(dirname "$0")/../ansible/inventory/hosts.ini}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/billing.pem}"
SSH_OPTS=(-o ConnectTimeout=8 -o ServerAliveInterval=5 -o ServerAliveCountMax=3 -o StrictHostKeyChecking=accept-new -o BatchMode=yes)

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
      if ($i ~ /^ansible_user=/)   { split($i, c, "="); user = c[2]; gsub(/"/, "", user) }
      if ($i == "hive_retired=true") retired = 1
    }
    if (name != "" && ip != "" && (!retired || ENVIRON["INCLUDE_RETIRED"] == "1")) print name "\t" ip "\t" user
  }
' "$INVENTORY")

if [ -z "$NODES" ]; then
  echo "no nodes parsed from $INVENTORY" >&2
  exit 1
fi

# ONLY must name probed inventory hosts exactly (comma-separated, spaces
# ignored): a typo, an alias or a retired host used to audit nothing and still
# print "clean".
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
  # own. Do NOT add `-n`: it would swallow that stdin. Bounded end to end: a
  # wedged node cannot hang the whole audit.
  ssh -i "$SSH_KEY" "${SSH_OPTS[@]}" "root@$1" 'timeout 60 sh -s' 2>/dev/null <<'REMOTE'
ns=$(awk '/^nameserver/ { printf "%s,", $2 }' /etc/resolv.conf 2>/dev/null | sed 's/,$//')
if ! command -v npm >/dev/null 2>&1; then
  echo "NO-NPM|-|-|-|-|$(curl -s -m 8 -I -o /dev/null -w '%{http_code}' https://registry.npmjs.org/typescript 2>/dev/null)|${ns}"
  exit 0
fi
# `2>/dev/null` + last line: npm prints update/engine warnings on stdout too.
reg=$(HOME=/root npm config get registry 2>/dev/null | tail -1 | tr -d '\r')
case "$reg" in http://*|https://*) ;; *) reg=https://registry.npmjs.org/ ;; esac
scheme=${reg%%://*}
rest=${reg#*://}; authority=${rest%%/*}
# userinfo (`user:token@host`) is never part of the host, and never printed.
hostport=${authority##*@}
case "$hostport" in
  \[*\]*) host=${hostport%%]*}; host=${host#[}; port=${hostport##*]}; port=${port#:} ;;
  *:*)    host=${hostport%%:*}; port=${hostport##*:} ;;
  *)      host=$hostport; port="" ;;
esac
[ -n "$port" ] || { [ "$scheme" = http ] && port=80 || port=443; }
# The host string comes from a config file and is interpolated into a shell
# command below: anything but a hostname/IP literal is reported, never run.
case "$host" in
  *[!A-Za-z0-9.:-]*|"") echo "MALFORMED|-|-|-|-|${ns}"; exit 0 ;;
esac
case "$port" in *[!0-9]*|"") echo "MALFORMED|-|-|-|-|${ns}"; exit 0 ;; esac
case "$host" in
  # An IP literal has nothing to resolve; getent returns 2 without a PTR.
  *[a-zA-Z]*) if getent hosts "$host" >/dev/null 2>&1; then dns=ok; else dns=FAIL; fi ;;
  *) dns=ip ;;
esac
if timeout 5 sh -c "exec 3<>/dev/tcp/$host/$port" 2>/dev/null; then tcp=ok; else tcp=FAIL; fi
shown=$scheme://$host; [ -n "${hostport##*:*}" ] || shown=$shown:$port
http=$(curl -s -m 8 -I -o /dev/null -w '%{http_code}' "${reg%/}/typescript" 2>/dev/null); http=${http:-000}
pub=$(curl -s -m 8 -I -o /dev/null -w '%{http_code}' https://registry.npmjs.org/typescript 2>/dev/null); pub=${pub:-000}
echo "${shown}|${dns}|${tcp}|${http}|${pub}|${ns}"
REMOTE
}

# A registry "answers" when a server replies at all: 2xx, a redirect, or an
# auth challenge (401/403) from a private/proxy registry. 000 = no answer,
# 404/5xx = answers but does not serve packuments.
answers() { case "$1" in 2??|3??|401|403) return 0 ;; *) return 1 ;; esac; }

printf "%-18s %-16s %-34s %-5s %-5s %-5s %-6s %s\n" NODE HOST_IP REGISTRY DNS TCP HTTP PUBLIC RESOLVERS
printf "%.0s-" {1..120} && echo

bad=0; audited=0; skipped=0
while IFS=$'\t' read -r name ip user; do
  [ -z "$name" ] && continue
  if [ -n "$ONLY" ] && [[ ",${ONLY}," != *",${name},"* ]]; then continue; fi
  if [ "${user:-root}" != root ]; then
    printf "%-18s %-16s %s\n" "$name" "$ip" "SKIPPED (ansible_user=$user is not root -- audit this host by hand)"
    skipped=$((skipped + 1))
    continue
  fi
  audited=$((audited + 1))
  out=$(probe "$ip")
  if [ -z "$out" ]; then
    printf "%-18s %-16s %s\n" "$name" "$ip" "UNREACHABLE (ssh failed -- try: ssh -i $SSH_KEY root@$ip true)"
    bad=1
    continue
  fi
  IFS='|' read -r reg dns tcp http pub ns <<< "$out"
  flag=""
  if [ "$reg" = NO-NPM ]; then
    flag="  <-- NO npm ON THIS NODE: every host-lane install fails"; bad=1
  elif [ "$reg" = MALFORMED ]; then
    flag="  <-- MALFORMED registry URL in the host npmrc: not probed"; bad=1
  elif [ "$dns" = FAIL ] || [ "$tcp" != ok ] || ! answers "$http"; then
    flag="  <-- REGISTRY DARK"
    answers "$pub" || flag="  <-- REGISTRY DARK, PUBLIC ALSO DARK"
    bad=1
  fi
  printf "%-18s %-16s %-34s %-5s %-5s %-5s %-6s %s%s\n" "$name" "$ip" "$reg" "$dns" "$tcp" "$http" "$pub" "$ns" "$flag"
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
  echo "DARK REGISTRY, NO NPM, UNAUDITED OR UNREACHABLE NODE ABOVE -- builds that need an uncached package will fail there."
  echo "(a dark registry with sane resolvers is the mirror; otherwise suspect /etc/resolv.conf -- 'nmcli general reload dns-rc' regenerates it on NetworkManager hosts)"
  exit 1
fi
echo "clean: every audited node resolves and reaches the registry its builds use."
