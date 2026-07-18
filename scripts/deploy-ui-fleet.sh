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

# SOURCE OF TRUTH: ansible/inventory/hosts.ini ([platform:children] -> the
# fc_kvm + fc_pvm groups, ansible_host= values). This array is a hand-copied
# mirror of those same 7 IPs, NOT auto-derived from the ini file, because the
# order below (fc-sanjose/control-plane-leader first, then the rest) is a
# deliberate rollout-safety curation that does not match hosts.ini's
# group/file order -- parsing it back out would silently reorder deploys.
# If a fleet node is added/removed/re-IP'd in hosts.ini, update this array in
# the same change, or this script will deploy to a stale host list. Re-check
# for drift with:
#   diff <(grep -oE 'ansible_host=[0-9.]+' ansible/inventory/hosts.ini | cut -d= -f2 | sort) \
#        <(awk '/^DEFAULT_HOSTS=\(/{f=1;next} /^\)/{f=0} f{print $1}' scripts/deploy-ui-fleet.sh | sort)
DEFAULT_HOSTS=(
  170.106.158.151  # fc-sanjose (control-plane leader)
  170.106.40.67    # fc-virginia-2
  43.128.46.225    # fc-hongkong
  43.152.247.70    # fc-bangkok
  43.166.206.175   # fc-virginia
  43.172.25.45     # fc-virginia-3
  43.173.78.95     # fc-sanjose-2
)
HOSTS=("${@:-${DEFAULT_HOSTS[@]}}")

ssh_opts=(-i "$PEM" -o StrictHostKeyChecking=no -o ConnectTimeout=8)

for host in "${HOSTS[@]}"; do
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
  '
  echo "==> $host: done"
  echo
done
