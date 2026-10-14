#!/usr/bin/env bash
# Pre-roll audit: will this node's secrets.rs/integrity_signer.rs key resolution boot cleanly?
#
# Why this exists: commit 7851352d97 made load_or_create_key/open_or_create FAIL CLOSED (exit
# the process / return a loud error) on a malformed HIVE_SECRET_KEY, a damaged secret.key, an
# unreadable integrity_signing.key, or a key that cannot be persisted -- see CHANGELOG.md "at-rest
# key resolution fails closed". That is correct behaviour for a node booting FRESH, but it means a
# node whose key was ALREADY in one of those states (before the fix ever shipped) will refuse to
# start on its next restart. Run this against the fleet BEFORE a roll: a node this script flags
# will not come back up afterward, and if that node is the control-plane leader or chain head, the
# roll takes the control plane down. This script changes nothing on any host.
#
# Checks per live host, read-only, NEVER printing a key or any /proc/*/environ content:
#   HIVE_SECRET_KEY      as the RUNNING hive-node process sees it (/proc/<MainPID>/environ, never
#                        the unit file, which can lie). Valid when absent, or exactly 64 lowercase
#                        hex characters. Reported as a verdict + length only.
#   HIVE_SECRET_KEY_OLD  same environ, comma-separated; every entry must be 64 lowercase hex.
#   $HIVE_DATA/secret.key       when present: readable by the service user, exactly 64 hex, mode 0600.
#   $HIVE_DATA/integrity_signing.key  when present: a readable regular file, valid hex starting with
#                        the PKCS8 DER SEQUENCE tag (0x30) -- a STRUCTURAL check only: this script
#                        does not link ring's Ed25519 parser, so it cannot fully repeat the real
#                        `Ed25519KeyPair::from_pkcs8` the Rust code runs; a value that passes this
#                        check can still be rejected at boot for a reason only that parser catches.
# Every reason string below matches the wording the Rust boot error uses, so a flag here and a boot
# failure after the roll read as the same finding.
#
# Usage:
#   scripts/audit-secret-keys.sh                      # whole fleet
#   ONLY=fc-sanjose,fc-virginia scripts/audit-secret-keys.sh   # hive_names, exact, comma-separated
#   INCLUDE_RETIRED=1 ...                              # also probe hive_retired=true hosts
#   ALLOW_SKIPPED=1 ...                                # accept hosts this script cannot audit
#   SSH_KEY=~/.ssh/other.pem scripts/audit-secret-keys.sh
#   scripts/audit-secret-keys.sh --local <data-dir> <environ-file>
#     # witness/local mode, no ssh, no fleet: checks <data-dir>/secret.key and
#     # <data-dir>/integrity_signing.key directly, and reads HIVE_SECRET_KEY /
#     # HIVE_SECRET_KEY_OLD from <environ-file> (NUL-separated KEY=VALUE records, the same shape
#     # /proc/<pid>/environ has -- construct one with `printf 'HIVE_SECRET_KEY=...\0' > file`).
# Exit status: 0 clean, 1 when any check fails, any host is unreachable, or was skipped (unless
# ALLOW_SKIPPED=1), 2 when nothing was audited (bad ONLY, empty selection, or --local given fewer
# than 2 args).
set -uo pipefail

# ---------------------------------------------------------------------------
# Shared check logic (POSIX sh): runs identically over ssh (against the real
# /proc/<pid>/environ and the real HIVE_DATA) and locally under --local
# (against a synthetic environ file and an explicit data-dir). Emits one
# "CHECK|STATUS|reason" line per check; STATUS is PASS or FAIL. Never echoes
# the content of ENVIRON_FILE, SECRET_KEY or the signing key.
read -r -d '' CHECK_SH <<'SH_EOF'
ENVIRON_FILE="$1"; DATA_DIR="$2"
is_hex64() { printf '%s' "$1" | grep -qE '^[0-9a-f]{64}$'; }

env_text=$(tr '\0' '\n' < "$ENVIRON_FILE" 2>/dev/null)
sk=$(printf '%s\n' "$env_text" | sed -n 's/^HIVE_SECRET_KEY=//p' | head -1)
sk_old=$(printf '%s\n' "$env_text" | sed -n 's/^HIVE_SECRET_KEY_OLD=//p' | head -1)

if [ -z "$sk" ]; then
  echo "HIVE_SECRET_KEY|PASS|absent (dev/file-key flow)"
elif is_hex64 "$sk"; then
  echo "HIVE_SECRET_KEY|PASS|present, 64 hex characters"
else
  echo "HIVE_SECRET_KEY|FAIL|is set but is not 64 hex characters (${#sk} chars) -- refusing to fall back to this node's own secret.key, which the rest of the fleet does not share"
fi

if [ -n "$sk_old" ]; then
  old_bad=0
  saved_ifs=$IFS; IFS=','
  for part in $sk_old; do
    IFS=$saved_ifs
    [ -n "$part" ] || continue
    is_hex64 "$part" || old_bad=1
  done
  IFS=$saved_ifs
  if [ "$old_bad" = 1 ]; then
    echo "HIVE_SECRET_KEY_OLD|FAIL|has an entry that is not 64 hex characters -- that entry can never open anything sealed under it"
  else
    echo "HIVE_SECRET_KEY_OLD|PASS|every entry is 64 hex characters"
  fi
fi

f="$DATA_DIR/secret.key"
if [ -e "$f" ]; then
  if [ ! -f "$f" ]; then
    echo "secret.key|FAIL|$f exists but is not a regular file"
  elif [ ! -r "$f" ]; then
    echo "secret.key|FAIL|$f exists but could not be read by the service user -- refusing to treat an existing key file as absent and replace it"
  else
    content=$(tr -d '\n\r' < "$f" 2>/dev/null)
    mode=$(stat -c '%a' "$f" 2>/dev/null || stat -f '%Lp' "$f" 2>/dev/null)
    if ! is_hex64 "$content"; then
      echo "secret.key|FAIL|$f exists but is not 64 hex characters (${#content} bytes) -- refusing to regenerate over it (every value already sealed under it would be orphaned)"
    elif [ "$mode" != "600" ]; then
      echo "secret.key|FAIL|$f is 64 hex characters but mode is $mode, not 600"
    else
      echo "secret.key|PASS|readable, 64 hex characters, mode 600"
    fi
  fi
else
  echo "secret.key|PASS|$f absent (a fresh one is generated on next boot, or HIVE_SECRET_KEY is set)"
fi

f="$DATA_DIR/integrity_signing.key"
if [ -e "$f" ]; then
  if [ ! -f "$f" ]; then
    echo "integrity_signing.key|FAIL|$f exists but is not a regular file -- refusing to overwrite this node's signing identity with a fresh key nobody has cross-checked"
  elif [ ! -r "$f" ]; then
    echo "integrity_signing.key|FAIL|$f exists but could not be read as text -- refusing to overwrite this node's signing identity with a fresh key nobody has cross-checked"
  else
    content=$(tr -d '\n\r' < "$f" 2>/dev/null)
    len=${#content}
    if [ "$len" = 0 ] || [ $((len % 2)) -ne 0 ] || ! printf '%s' "$content" | grep -qiE '^[0-9a-f]+$'; then
      echo "integrity_signing.key|FAIL|$f exists but is not valid hex"
    else
      first=$(printf '%s' "$content" | cut -c1-2 | tr 'A-F' 'a-f')
      if [ "$first" != "30" ]; then
        echo "integrity_signing.key|FAIL|$f is valid hex but does not start with the PKCS8 DER SEQUENCE tag (30) -- likely not a parseable signing key"
      else
        echo "integrity_signing.key|PASS|readable, valid hex, starts with the PKCS8 DER SEQUENCE tag (structural check only, not a full PKCS8/Ed25519 parse)"
      fi
    fi
  fi
else
  echo "integrity_signing.key|PASS|$f absent (a fresh one is generated on next boot)"
fi
SH_EOF

report() {
  # <label> <ip-or-local> <checks output>
  local label=$1 where=$2 out=$3
  local bad_here=0
  while IFS='|' read -r check status reason; do
    [ -z "$check" ] && continue
    if [ "$status" = FAIL ]; then
      printf "%-18s %-16s FAIL  %-22s %s\n" "$label" "$where" "$check" "$reason"
      bad_here=1
    else
      printf "%-18s %-16s pass  %-22s %s\n" "$label" "$where" "$check" "$reason"
    fi
  done <<< "$out"
  return $bad_here
}

# ---------------------------------------------------------------------------
# --local <data-dir> <environ-file>: witness/local mode, no ssh, no inventory.
if [ "${1:-}" = "--local" ]; then
  DATA_DIR="${2:?usage: audit-secret-keys.sh --local <data-dir> <environ-file>}"
  ENVIRON_FILE="${3:?usage: audit-secret-keys.sh --local <data-dir> <environ-file>}"
  [ -f "$ENVIRON_FILE" ] || { echo "environ-file not found: $ENVIRON_FILE" >&2; exit 2; }
  OUT=$(sh -c "$CHECK_SH" sh "$ENVIRON_FILE" "$DATA_DIR")
  printf "%-18s %-16s %-6s %-22s %s\n" NODE WHERE STATUS CHECK REASON
  printf "%.0s-" {1..100} && echo
  if report local "$DATA_DIR" "$OUT"; then
    echo; echo "clean: $DATA_DIR / $ENVIRON_FILE pass every check."
    exit 0
  else
    echo; echo "FINDING ABOVE -- a node booting with this HIVE_DATA/environ would refuse to start (or already fails the pre-boot check this script repeats)."
    exit 1
  fi
fi

# ---------------------------------------------------------------------------
# Fleet mode.
INVENTORY="${INVENTORY:-$(dirname "$0")/../ansible/inventory/hosts.ini}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/billing.pem}"
SSH_OPTS=(-o ConnectTimeout=8 -o ServerAliveInterval=5 -o ServerAliveCountMax=3 -o StrictHostKeyChecking=accept-new -o BatchMode=yes)

if [ ! -f "$INVENTORY" ]; then
  echo "inventory not found: $INVENTORY" >&2
  exit 1
fi

# name<TAB>ip<TAB>ssh-user, skipping comments/group headers/vars lines and every `hive_retired=true`
# host (same parse and same rule as scripts/audit-build-registry.sh; INCLUDE_RETIRED=1 puts them back).
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

ONLY="${ONLY:-}"; ONLY="${ONLY// /}"
if [ -n "$ONLY" ]; then
  IFS=',' read -ra only_names <<< "$ONLY"
  for only_name in "${only_names[@]}"; do
    printf '%s\n' "$NODES" | cut -f1 | grep -qxF -- "$only_name" \
      || { echo "ONLY names an unknown or retired host: $only_name" >&2; exit 2; }
  done
fi

CHECK_SH_B64=$(printf '%s' "$CHECK_SH" | base64 | tr -d '\n')

probe() {
  # The remote script travels on stdin (`sh -s`): no quoting of its own payload needed. Finds
  # hive-node's MainPID the AGENTS.md way (/proc/<pid>/environ, never the unit file: the file can
  # lie); falls back to `pgrep -x hive-cloud` when systemctl is unavailable. HIVE_DATA is read from
  # the SAME live environ, falling back to $HOME/.hive-cloud (also from that environ) like
  # persist.rs's own default. Bounded end to end: a wedged node cannot hang the whole audit.
  ssh -i "$SSH_KEY" "${SSH_OPTS[@]}" "root@$1" 'timeout 60 sh -s' 2>/dev/null <<REMOTE
pid=\$(systemctl show hive-node -p MainPID --value 2>/dev/null)
[ -n "\$pid" ] && [ "\$pid" != 0 ] || pid=\$(pgrep -x hive-cloud | head -1)
[ -n "\$pid" ] || { echo "NO-PROCESS|FAIL|hive-node/hive-cloud is not running on this host"; exit 0; }
[ -r "/proc/\$pid/environ" ] || { echo "NO-PROCESS|FAIL|/proc/\$pid/environ is not readable (not root?)"; exit 0; }
env_text=\$(tr '\\0' '\\n' < "/proc/\$pid/environ" 2>/dev/null)
home=\$(printf '%s\\n' "\$env_text" | sed -n 's/^HOME=//p' | head -1); home=\${home:-/root}
data_dir=\$(printf '%s\\n' "\$env_text" | sed -n 's/^HIVE_DATA=//p' | head -1); data_dir=\${data_dir:-\$home/.hive-cloud}
echo "$CHECK_SH_B64" | base64 -d > /tmp/.audit-secret-keys-check.\$\$.sh
sh /tmp/.audit-secret-keys-check.\$\$.sh "/proc/\$pid/environ" "\$data_dir"
rm -f /tmp/.audit-secret-keys-check.\$\$.sh
REMOTE
}

printf "%-18s %-16s %-6s %-22s %s\n" NODE IP STATUS CHECK REASON
printf "%.0s-" {1..100} && echo

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
  if ! report "$name" "$ip" "$out"; then
    bad=1
  fi
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
  echo "FINDING, UNAUDITED OR UNREACHABLE NODE ABOVE -- that node will refuse to boot (or cannot be proven safe) under the fail-closed key resolution. Fix it BEFORE rolling, or the control plane may not come back up if it is the leader/chain head."
  exit 1
fi
echo "clean: every audited node's secret-key resolution passes every check."
