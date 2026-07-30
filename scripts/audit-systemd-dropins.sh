#!/usr/bin/env bash
# Find systemd drop-in config drift on hive-node across the fleet.
#
# Two real incidents this catches, both witnessed in production:
#
#  1. SHADOWED ENV VAR. Drop-ins are applied in lexical filename order and the
#     LAST assignment of a key wins. A stale hand-written `local.conf` silently
#     overrode a newly-added `geo.conf` (l > g), so a correct fix appeared to do
#     nothing. Nothing warns about this -- systemd just takes the last value.
#     Any key set in more than one drop-in is reported here with the winner
#     marked, because that is the only way to see it without reading ~18 files
#     by hand.
#
#  2. LIVE PROCESS DISAGREEING WITH THE FILES. A drop-in with a typo'd value
#     keyed an entire subsystem on a domain nobody owned while the main unit
#     file read correct -- AGENTS.md's standing advice is "verify with
#     /proc/<pid>/environ, never the unit file". This compares what systemd
#     would compute now against what the RUNNING process actually carries, so a
#     process still holding pre-edit values (config changed, never restarted)
#     shows up as drift instead of as a mystery weeks later.
#
# Read-only. Changes nothing, on any host.
#
# Usage:
#   scripts/audit-systemd-dropins.sh              # whole fleet
#   scripts/audit-systemd-dropins.sh sj cvmsj1    # named inventory hosts
set -uo pipefail

INVENTORY="${INVENTORY:-$(dirname "$0")/../ansible/inventory/hosts.ini}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/billing.pem}"
UNIT="${UNIT:-hive-node}"
SSH_OPTS=(-n -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new -o BatchMode=yes)

declare -a WANT=("$@")

# Compares systemd's computed Environment against the live process environ.
# Lives here as a file (base64'd onto the target below) rather than as a
# heredoc inside the ssh command string, because a heredoc there is expanded by
# the LOCAL shell and its Python punctuation becomes bash syntax errors.
#
# shlex, not a space split: systemd emits Environment= as one space-separated
# line and shell-quotes any value containing spaces. HIVE_FC_BOOT_ARGS
# ("console=ttyS0 reboot=k panic=1 ...") is a real one, and splitting it
# naively invents keys (reboot, panic, pci, root) that are then all reported as
# false drift.
read -r -d '' COMPARE_PY <<'PYEOF'
import shlex, sys
pid, declpath = sys.argv[1], sys.argv[2]
declared = {}
for tok in shlex.split(open(declpath).read()):
    if "=" in tok:
        k, _, v = tok.partition("=")
        if k and (k[0].isalpha() or k[0] == "_"):
            declared[k] = v
live = {}
with open("/proc/%s/environ" % pid, "rb") as f:
    for chunk in f.read().split(b"\0"):
        if not chunk:
            continue
        k, _, v = chunk.decode("utf-8", "replace").partition("=")
        if k:
            live[k] = v
def t(s):
    return (s[:60] + "...") if len(s) > 60 else s
drift = 0
for k in sorted(declared):
    want = declared[k]
    got = live.get(k)
    if got != want:
        print("DRIFT %s: declared='%s' running='%s'" % (k, t(want), "<ABSENT>" if got is None else t(got)))
        drift += 1
if drift == 0:
    print("(none - running process matches declared config)")
PYEOF
COMPARE_B64=$(printf '%s' "$COMPARE_PY" | base64 | tr -d '\n')

NODES=$(awk '
  /^\[/    { in_vars = ($0 ~ /:vars\]/); next }
  /^#/     { next }
  /^[ \t]*$/ { next }
  in_vars  { next }
  {
    host = $1; ip = ""
    for (i = 1; i <= NF; i++) if ($i ~ /^ansible_host=/) { split($i, b, "="); ip = b[2] }
    if (host != "" && ip != "") print host "\t" ip
  }
' "$INVENTORY")

drift_total=0

while IFS=$'\t' read -r host ip; do
  [ -z "$host" ] && continue
  if [ ${#WANT[@]} -gt 0 ]; then
    match=0
    for w in "${WANT[@]}"; do [ "$w" = "$host" ] && match=1; done
    [ $match = 0 ] && continue
  fi

  echo "=============================================================="
  echo "  $host  ($ip)"
  echo "=============================================================="

  out=$(ssh -i "$SSH_KEY" "${SSH_OPTS[@]}" "root@$ip" \
    "UNIT='$UNIT'; PYB64='$COMPARE_B64'; "'
    D=/etc/systemd/system/$UNIT.service.d
    if [ ! -d "$D" ]; then echo "NO_DROPIN_DIR"; exit 0; fi

    echo "--- drop-ins (lexical order = apply order; later wins) ---"
    ls -1 "$D"/*.conf 2>/dev/null | xargs -n1 basename 2>/dev/null

    echo "--- env keys set in MORE THAN ONE drop-in (shadowing) ---"
    for f in $(ls -1 "$D"/*.conf 2>/dev/null); do
      b=$(basename "$f")
      sed -n "s/^[[:space:]]*Environment=\"\{0,1\}\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p" "$f" \
        | while read -r k; do echo "$k $b"; done
    done | awk "
      { files[\$1] = (\$1 in files ? files[\$1] \" \" \$2 : \$2); n[\$1]++ }
      END {
        found = 0
        for (k in n) if (n[k] > 1) {
          split(files[k], parts, \" \")
          print k \" set in: \" files[k] \"  -> WINS: \" parts[n[k]]
          found = 1
        }
        if (!found) print \"(none - every key is set in exactly one drop-in)\"
      }
    " | sort

    echo "--- running process vs declared config ---"
    PID=$(systemctl show "$UNIT" -p MainPID --value 2>/dev/null)
    if [ -z "$PID" ] || [ "$PID" = 0 ]; then
      echo "unit not running - cannot compare live env"
    else
      systemctl show "$UNIT" -p Environment --value 2>/dev/null > /tmp/.decl.$$
      echo "$PYB64" | base64 -d > /tmp/.cmp.$$.py
      python3 /tmp/.cmp.$$.py "$PID" /tmp/.decl.$$
      rm -f /tmp/.decl.$$ /tmp/.cmp.$$.py
    fi
  ' 2>/dev/null)

  if [ -z "$out" ]; then
    echo "  UNREACHABLE"
  else
    echo "$out" | sed 's/^/  /'
    if echo "$out" | grep -q '^DRIFT '; then
      drift_total=$((drift_total + 1))
    fi
  fi
  echo
done <<< "$NODES"

echo "nodes with live-vs-declared drift: $drift_total"
echo
echo "Note: a key in more than one drop-in is not automatically a bug (a"
echo "deliberate override is legitimate), but it IS the shape that hid a real"
echo "misconfiguration -- confirm the winner is the value you intended."
