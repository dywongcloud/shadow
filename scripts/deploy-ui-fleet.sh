#!/usr/bin/env bash
# Fleet-wide rollout for the ui/ dashboard (Next.js, systemd hive-ui.service,
# next-server on 127.0.0.1:3002 behind HIVE_DASHBOARD_UPSTREAM).
#
# A backend (hive-cloud binary) rollout does NOT deploy ui/ changes -- they
# are two independently-running services on each node. Run this any time a
# commit touches ui/ so the fleet doesn't silently keep serving a stale build
# (this is what happened for the Data Browser Tables view: the backend was
# fanned out fleet-wide but ui/ never was, so the feature was invisible on
# the real public dashboard for days).
#
# One host at a time (never all at once) -- matches the existing
# ansible/playbooks/platform-only.yml serial:1 discipline for backend
# rollouts, so the dashboard is never down fleet-wide during a bad build.
#
# Usage: scripts/deploy-ui-fleet.sh [host ...]   (defaults to the 7 fleet nodes)
set -euo pipefail

PEM="${HIVE_FLEET_PEM:-$HOME/Documents/billing.pem}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# SOURCE OF TRUTH: ansible/inventory/hosts.ini. The host list is DERIVED from it
# at runtime rather than hand-mirrored here -- a hand-copied array drifts, and it
# did: the three GPU nodes were added to the fleet and silently never received
# the dashboard because this array still listed the original seven, while the
# script exited 0 as though it had done the work.
#
# Ordering intent is preserved programmatically instead of by hand-curation: the
# control-plane leader (first name in hive_cp_owner_chain) is deployed FIRST,
# then every other host in inventory order. That was the only reason the array
# existed, so deriving membership costs nothing.
INVENTORY="$REPO_ROOT/ansible/inventory/hosts.ini"
[ -f "$INVENTORY" ] || { echo "inventory not found: $INVENTORY" >&2; exit 1; }

# name -> ip, plus the leader's ip resolved via hive_cp_owner_chain's first entry.
mapfile -t DEFAULT_HOSTS < <(
  awk '
    /^hive_cp_owner_chain=/ { sub(/^hive_cp_owner_chain=/,""); split($0, c, ","); leader_name=c[1] }
    /ansible_host=/ {
      ip=""; name="";
      for (i=1; i<=NF; i++) {
        if ($i ~ /^ansible_host=/) { split($i, a, "="); ip=a[2] }
        if ($i ~ /^hive_name=/)    { split($i, b, "="); name=b[2] }
      }
      if (ip != "") { ips[++n]=ip; names[n]=name }
    }
    END {
      for (i=1; i<=n; i++) if (names[i] == leader_name) print ips[i];
      for (i=1; i<=n; i++) if (names[i] != leader_name) print ips[i];
    }
  ' "$INVENTORY"
)
[ ${#DEFAULT_HOSTS[@]} -gt 0 ] || { echo "no ansible_host entries parsed from $INVENTORY" >&2; exit 1; }

HOSTS=("${@:-${DEFAULT_HOSTS[@]}}")

ssh_opts=(-i "$PEM" -o StrictHostKeyChecking=no -o ConnectTimeout=8)

# Hosts that failed any step. The loop deliberately continues past a failure so
# one bad node doesn't strand the rest, but the script MUST exit non-zero at the
# end -- it previously returned 0 even when a host had no nodejs installed at
# all, so "npm ci + next build" silently did nothing and the operator saw a
# clean run while that node kept serving a stale dashboard (or none).
failed=()

for host in "${HOSTS[@]}"; do
  echo "==> $host: preflight (node, npm, hive-ui unit)"
  if ! ssh "${ssh_opts[@]}" "root@$host" '
      command -v node >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: nodejs not installed"; exit 1; }
      command -v npm  >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: npm not installed"; exit 1; }
      [ -f /etc/systemd/system/hive-ui.service ] || { echo "PREFLIGHT FAIL: hive-ui.service unit missing"; exit 1; }
      echo "  node $(node --version), npm $(npm --version), unit present"
    '; then
    echo "==> $host: SKIPPED (preflight failed)"
    failed+=("$host")
    echo
    continue
  fi

  echo "==> $host: syncing ui/ source (preserving .env.local + node_modules + .next)"
  rsync -az --delete \
    --exclude node_modules \
    --exclude .next \
    --exclude '.env.local*' \
    -e "ssh ${ssh_opts[*]}" \
    "$REPO_ROOT/ui/" "root@$host:/root/hive/ui/"

  echo "==> $host: npm ci + next build"
  ssh "${ssh_opts[@]}" "root@$host" '
    set -e
    cd /root/hive/ui
    old_build_id="$(cat .next/BUILD_ID 2>/dev/null || echo none)"
    npm ci
    npm run build
    new_build_id="$(cat .next/BUILD_ID)"
    echo "build id: $old_build_id -> $new_build_id"
    systemctl restart hive-ui
    sleep 2
    systemctl is-active hive-ui
    curl -fsS -o /dev/null -w "local :3002 status=%{http_code}\n" http://127.0.0.1:3002/ || true
  ' || { echo "==> $host: FAILED (build/restart step)"; failed+=("$host"); echo; continue; }
  echo "==> $host: done"
  echo
done

if [ ${#failed[@]} -gt 0 ]; then
  echo "DEPLOY INCOMPLETE — ${#failed[@]} host(s) failed: ${failed[*]}" >&2
  exit 1
fi
echo "All ${#HOSTS[@]} host(s) deployed."
