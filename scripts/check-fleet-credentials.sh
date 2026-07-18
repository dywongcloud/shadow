#!/usr/bin/env bash
# Fleet-wide credential VERIFICATION (read-only, boolean presence only).
#
# For every fleet node, checks whether each required credential env var is
# SET and non-empty:
#   dashboard side  -- /root/hive/ui/.env.local:
#     COMPOSIO_API_KEY, GITHUB_APP_CLIENT_ID, GITHUB_APP_CLIENT_SECRET,
#     GITHUB_APP_REDIRECT_URI, HIVE_GITHUB_AUTH_CONFIG_ID
#   backend side    -- hive-node.service environment (systemctl show +
#                       unit/drop-in Environment= lines + EnvironmentFile=):
#     GITHUB_TOKEN, GITHUB_WEBHOOK_SECRET, HIVE_SECRET_KEY
#
# SAFETY (hard requirement): this script NEVER prints, logs, or otherwise
# outputs actual secret VALUES -- only "present"/"MISSING"/"UNREACHABLE" per
# var, per node. The remote-side checks read secret values into transient
# remote shell variables purely to test non-emptiness and never echo them.
# Any change to this script must preserve that invariant.
#
# READ-ONLY: makes no writes to any fleet node (no files touched, no
# services restarted). Safe to run at any time, does not require the
# explicit-approval gate that write/restart actions to the fleet need.
#
# Usage:
#   scripts/check-fleet-credentials.sh              # every node in hosts.ini
#   scripts/check-fleet-credentials.sh <ip> [ip...] # only the given hosts
#
# Exit status: 0 if every var is present on every reachable node, 1 if any
# var is MISSING or any node is UNREACHABLE (so this can also be used as a
# fleet health check in automation).
set -euo pipefail

PEM="${HIVE_FLEET_PEM:-$HOME/Documents/billing.pem}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOSTS_INI="$REPO_ROOT/ansible/inventory/hosts.ini"

DASHBOARD_VARS=(
  COMPOSIO_API_KEY
  GITHUB_APP_CLIENT_ID
  GITHUB_APP_CLIENT_SECRET
  GITHUB_APP_REDIRECT_URI
  HIVE_GITHUB_AUTH_CONFIG_ID
)
BACKEND_VARS=(
  GITHUB_TOKEN
  GITHUB_WEBHOOK_SECRET
  HIVE_SECRET_KEY
)
ALL_VARS=("${DASHBOARD_VARS[@]}" "${BACKEND_VARS[@]}")

# --- host list ---------------------------------------------------------
# alias|ip|display_name triples. hosts.ini (ansible/inventory/hosts.ini) is
# the single authoritative fleet host list; explicit args on the command
# line override it (matches deploy-ui-fleet.sh's "[host ...]" usage); a
# hardcoded fallback (same 7 nodes deploy-ui-fleet.sh defaults to) covers
# the case hosts.ini is missing/unreadable.
declare -a HOST_TRIPLES=()

if [[ $# -gt 0 ]]; then
  for h in "$@"; do
    HOST_TRIPLES+=("$h|$h|$h")
  done
elif [[ -r "$HOSTS_INI" ]]; then
  while IFS= read -r line; do
    [[ "$line" =~ ^\[ ]] && continue
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ansible_host= ]] || continue
    alias_name="$(awk '{print $1}' <<<"$line")"
    ip="$(grep -oE 'ansible_host=[^ ]+' <<<"$line" | cut -d= -f2)"
    hive_name="$(grep -oE 'hive_name=[^ ]+' <<<"$line" | cut -d= -f2)"
    [[ -z "$hive_name" ]] && hive_name="$alias_name"
    [[ -z "$ip" ]] && continue
    HOST_TRIPLES+=("$alias_name|$ip|$hive_name")
  done < "$HOSTS_INI"
fi

if [[ ${#HOST_TRIPLES[@]} -eq 0 ]]; then
  echo "==> $HOSTS_INI unreadable/empty -- falling back to hardcoded default hosts" >&2
  HOST_TRIPLES=(
    "sj|170.106.158.151|fc-sanjose"
    "va2|170.106.40.67|fc-virginia-2"
    "hk|43.128.46.225|fc-hongkong"
    "bkk|43.152.247.70|fc-bangkok"
    "va|43.166.206.175|fc-virginia"
    "va3|43.172.25.45|fc-virginia-3"
    "sj2|43.173.78.95|fc-sanjose-2"
  )
fi

ssh_opts=(-i "$PEM" -o StrictHostKeyChecking=no -o ConnectTimeout=8 -o BatchMode=yes)

# --- remote check script -------------------------------------------------
# Runs entirely on the remote node via `bash -s` over stdin. Only ever
# echoes "DASH:<VAR>=PRESENT|MISSING" / "BACK:<VAR>=PRESENT|MISSING" tokens.
# Secret values are read into transient local shell variables purely to
# test non-emptiness and are NEVER echoed, printed, logged, or returned.
read -r -d '' REMOTE_SCRIPT <<'REMOTE_EOF' || true
set -u

check_dashboard() {
  local env_file="/root/hive/ui/.env.local"
  local vars=(COMPOSIO_API_KEY GITHUB_APP_CLIENT_ID GITHUB_APP_CLIENT_SECRET GITHUB_APP_REDIRECT_URI HIVE_GITHUB_AUTH_CONFIG_ID)
  local var val

  if [[ ! -r "$env_file" ]]; then
    for var in "${vars[@]}"; do
      echo "DASH:${var}=MISSING"
    done
    return
  fi

  for var in "${vars[@]}"; do
    val="$(grep -E "^${var}=" "$env_file" 2>/dev/null | tail -n1 | cut -d= -f2-)"
    val="${val%\"}"; val="${val#\"}"
    val="${val%\'}"; val="${val#\'}"
    if [[ -n "$val" ]]; then
      echo "DASH:${var}=PRESENT"
    else
      echo "DASH:${var}=MISSING"
    fi
    unset val
  done
}

check_backend() {
  local vars=(GITHUB_TOKEN GITHUB_WEBHOOK_SECRET HIVE_SECRET_KEY)
  local var found f
  local svc_env="" svc_envfiles="" frag_path="" dropin_paths=""

  if command -v systemctl >/dev/null 2>&1; then
    svc_env="$(systemctl show hive-node --property=Environment --value 2>/dev/null || true)"
    svc_envfiles="$(systemctl show hive-node --property=EnvironmentFiles --value 2>/dev/null || true)"
    frag_path="$(systemctl show hive-node --property=FragmentPath --value 2>/dev/null || true)"
    dropin_paths="$(systemctl show hive-node --property=DropInPaths --value 2>/dev/null || true)"
  fi

  if [[ -z "$frag_path" && -z "$svc_env" && -z "$svc_envfiles" ]]; then
    for var in "${vars[@]}"; do
      echo "BACK:${var}=MISSING"
    done
    return
  fi

  # Literal Environment= lines from the main unit file + any drop-ins
  # (belt-and-suspenders alongside `systemctl show`, per the unit/drop-in
  # file read the task calls out as an acceptable check surface).
  local unit_env_lines=""
  for f in "$frag_path" $dropin_paths; do
    [[ -n "$f" && -r "$f" ]] || continue
    unit_env_lines="${unit_env_lines}
$(grep -E '^[[:space:]]*Environment=' "$f" 2>/dev/null || true)"
  done

  # Referenced EnvironmentFile= paths (strip a leading "-" ignore-errors
  # marker; systemctl show renders unresolved/missing files as "(path)").
  local envfile_content=""
  for f in $svc_envfiles; do
    f="${f#-}"
    [[ "$f" == "("* ]] && continue
    [[ -r "$f" ]] || continue
    envfile_content="${envfile_content}
$(cat "$f" 2>/dev/null || true)"
  done

  for var in "${vars[@]}"; do
    found=0
    if echo "$svc_env" | grep -qE "(^| )${var}=[^ ]"; then
      found=1
    elif echo "$unit_env_lines" | grep -qE "Environment=\"?${var}=.+"; then
      found=1
    elif echo "$envfile_content" | grep -qE "^${var}=.+"; then
      found=1
    fi
    if [[ "$found" -eq 1 ]]; then
      echo "BACK:${var}=PRESENT"
    else
      echo "BACK:${var}=MISSING"
    fi
  done
  unset svc_env svc_envfiles envfile_content unit_env_lines
}

check_dashboard
check_backend
REMOTE_EOF

# --- run against every node ----------------------------------------------
declare -A RESULTS   # key "alias:VAR" -> PRESENT|MISSING|UNREACHABLE
declare -a ORDERED=()

for triple in "${HOST_TRIPLES[@]}"; do
  IFS='|' read -r alias_name ip disp_name <<<"$triple"
  ORDERED+=("$alias_name")
  echo "==> $disp_name ($ip): checking credentials..." >&2

  if output="$(ssh "${ssh_opts[@]}" "root@$ip" bash -s <<<"$REMOTE_SCRIPT" 2>/dev/null)"; then
    while IFS= read -r line; do
      [[ "$line" == DASH:* || "$line" == BACK:* ]] || continue
      kv="${line#*:}"
      var="${kv%%=*}"
      status="${kv#*=}"
      RESULTS["${alias_name}:${var}"]="$status"
    done <<<"$output"
  else
    echo "    UNREACHABLE (ssh failed)" >&2
    for var in "${ALL_VARS[@]}"; do
      RESULTS["${alias_name}:${var}"]="UNREACHABLE"
    done
  fi
done

# --- summary: full per-node / per-var table -------------------------------
echo
echo "================================================================================"
echo " Fleet Credential Verification -- presence/absence only, no secret values shown"
echo "================================================================================"

any_gap=0
for alias_name in "${ORDERED[@]}"; do
  disp_name="$alias_name"
  for triple in "${HOST_TRIPLES[@]}"; do
    IFS='|' read -r t_alias t_ip t_name <<<"$triple"
    [[ "$t_alias" == "$alias_name" ]] && disp_name="$t_name ($t_ip)"
  done
  echo
  echo "-- $disp_name --"
  printf "  %-30s %-10s %s\n" "VARIABLE" "SCOPE" "STATUS"
  for var in "${DASHBOARD_VARS[@]}"; do
    status="${RESULTS["${alias_name}:${var}"]:-UNKNOWN}"
    [[ "$status" != "PRESENT" ]] && any_gap=1
    printf "  %-30s %-10s %s\n" "$var" "dashboard" "$status"
  done
  for var in "${BACKEND_VARS[@]}"; do
    status="${RESULTS["${alias_name}:${var}"]:-UNKNOWN}"
    [[ "$status" != "PRESENT" ]] && any_gap=1
    printf "  %-30s %-10s %s\n" "$var" "backend" "$status"
  done
done

# --- summary: missing-only quick-scan -------------------------------------
echo
echo "================================================================================"
echo " Quick scan -- nodes with any MISSING/UNREACHABLE credential"
echo "================================================================================"
if [[ "$any_gap" -eq 0 ]]; then
  echo "  (none -- every checked var is PRESENT on every reachable node)"
else
  for alias_name in "${ORDERED[@]}"; do
    disp_name="$alias_name"
    for triple in "${HOST_TRIPLES[@]}"; do
      IFS='|' read -r t_alias t_ip t_name <<<"$triple"
      [[ "$t_alias" == "$alias_name" ]] && disp_name="$t_name ($t_ip)"
    done
    node_gaps=()
    for var in "${ALL_VARS[@]}"; do
      status="${RESULTS["${alias_name}:${var}"]:-UNKNOWN}"
      [[ "$status" != "PRESENT" ]] && node_gaps+=("${var}=${status}")
    done
    if [[ ${#node_gaps[@]} -gt 0 ]]; then
      echo "  $disp_name:"
      for g in "${node_gaps[@]}"; do
        echo "    - $g"
      done
    fi
  done
fi
echo

if [[ "$any_gap" -eq 0 ]]; then
  exit 0
else
  exit 1
fi
