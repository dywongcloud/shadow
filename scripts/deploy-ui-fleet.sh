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
# ONE BUILD, MANY NODES -- the build runs ONCE (on the first/leader host) and
# the resulting .next artifact is distributed byte-identically to every other
# node. Per-node `next build` is FORBIDDEN here: Next/Turbopack builds are not
# deterministic, so 13 nodes each building the same source produced 13
# DIFFERENT chunk sets behind one round-robin hostname -- an HTML document
# served by node A referenced content-hashed chunks that 404'd on node B, and
# a browser that cached that document (or bounced nodes mid-load, which mobile
# radios do constantly) rendered a permanently blank page. Live-witnessed as
# the mobile "endless flicker": 9 chunk 404s under a cached shell, 3/3
# navigation cycles. One artifact = one BUILD_ID = every chunk resolves on
# every node, and /sw.js stays byte-identical so the PWA update engine stays
# dormant outside real deploys.
#
# Still one host at a time for the restart (dashboard never down fleet-wide).
#
# Usage: scripts/deploy-ui-fleet.sh [host ...]   (defaults to the fleet inventory)
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

# One multiplexed TCP connection per host, reused by every ssh/rsync/scp step.
# Without this the script opens 4+ rapid connections per host while background
# automation holds its own — sshd's MaxStartups (10:30:100) then drops pre-auth
# connections ("Connection closed by <ip> port 22"), which live-failed a whole
# 12-host distribution pass at preflight while every host was actually fine.
CTL_DIR="$(mktemp -d)"
ssh_opts=(-i "$PEM" -o StrictHostKeyChecking=no -o ConnectTimeout=8 \
  -o ControlMaster=auto -o "ControlPath=$CTL_DIR/%r@%h" -o ControlPersist=120)
trap 'rm -rf "$CTL_DIR"' EXIT

# Hosts that failed any step. The loop deliberately continues past a failure so
# one bad node doesn't strand the rest, but the script MUST exit non-zero at the
# end -- it previously returned 0 even when a host had no nodejs installed at
# all, so "npm ci + next build" silently did nothing and the operator saw a
# clean run while that node kept serving a stale dashboard (or none).
failed=()

ARTIFACT="$(mktemp -d)/next-build.tar.gz"
BUILD_HOST=""
CANON_BUILD_ID=""

sync_source() {
  local host="$1"
  rsync -az --delete \
    --exclude node_modules \
    --exclude .next \
    --exclude '.env.local*' \
    -e "ssh ${ssh_opts[*]}" \
    "$REPO_ROOT/ui/" "root@$host:/root/hive/ui/"
}

preflight() {
  local host="$1"
  # One retry after a short pause: a MaxStartups drop is momentary by nature.
  ssh "${ssh_opts[@]}" "root@$host" true 2>/dev/null || sleep 5
  ssh "${ssh_opts[@]}" "root@$host" '
    command -v node >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: nodejs not installed"; exit 1; }
    command -v npm  >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: npm not installed"; exit 1; }
    [ -f /etc/systemd/system/hive-ui.service ] || { echo "PREFLIGHT FAIL: hive-ui.service unit missing"; exit 1; }
    echo "  node $(node --version), npm $(npm --version), unit present"
  '
}

# ---- phase 1: build ONCE on the first reachable host ----
for host in "${HOSTS[@]}"; do
  echo "==> $host: preflight (build-host candidate)"
  if ! preflight "$host"; then
    echo "==> $host: SKIPPED as build host (preflight failed)"; failed+=("$host"); continue
  fi
  echo "==> $host: syncing ui/ source"
  sync_source "$host"
  echo "==> $host: npm ci + next build (the ONE canonical build)"
  if ssh "${ssh_opts[@]}" "root@$host" '
      set -e
      cd /root/hive/ui
      npm ci
      npm run build
      echo "build id: $(cat .next/BUILD_ID)"
      # Package everything `next start` needs; the cache dir is node-local
      # build state and huge -- never ship it.
      tar -C .next --exclude cache -czf /root/hive/ui/.next-artifact.tar.gz .
      systemctl restart hive-ui
      sleep 2
      systemctl is-active hive-ui
      curl -fsS -o /dev/null -w "local :3002 status=%{http_code}\n" http://127.0.0.1:3002/ || true
    '; then
    BUILD_HOST="$host"
    CANON_BUILD_ID="$(ssh "${ssh_opts[@]}" "root@$host" 'cat /root/hive/ui/.next/BUILD_ID')"
    echo "==> $host: canonical build $CANON_BUILD_ID; pulling artifact"
    scp -q "${ssh_opts[@]}" "root@$host:/root/hive/ui/.next-artifact.tar.gz" "$ARTIFACT"
    ssh "${ssh_opts[@]}" "root@$host" 'rm -f /root/hive/ui/.next-artifact.tar.gz'
    break
  else
    echo "==> $host: FAILED to build; trying the next host as build host"
    failed+=("$host")
  fi
done
[ -n "$BUILD_HOST" ] || { echo "FATAL: no host could produce the canonical build" >&2; exit 1; }

# ---- phase 2: distribute the SAME artifact to every other host ----
for host in "${HOSTS[@]}"; do
  [ "$host" = "$BUILD_HOST" ] && continue
  echo "==> $host: preflight"
  if ! preflight "$host"; then
    echo "==> $host: SKIPPED (preflight failed)"; failed+=("$host"); echo; continue
  fi
  echo "==> $host: syncing ui/ source"
  if ! sync_source "$host"; then
    echo "==> $host: FAILED (rsync)"; failed+=("$host"); echo; continue
  fi
  echo "==> $host: pushing canonical artifact $CANON_BUILD_ID"
  if ! scp -q "${ssh_opts[@]}" "$ARTIFACT" "root@$host:/root/hive/ui/.next-artifact.tar.gz"; then
    echo "==> $host: FAILED (artifact push)"; failed+=("$host"); echo; continue
  fi
  if ! ssh "${ssh_opts[@]}" "root@$host" '
      set -e
      cd /root/hive/ui
      npm ci
      rm -rf .next.new && mkdir .next.new
      tar -C .next.new -xzf .next-artifact.tar.gz
      rm -f .next-artifact.tar.gz
      # Atomic-ish swap so a crash mid-extract never leaves a half build live.
      rm -rf .next.old
      [ -d .next ] && mv .next .next.old
      mv .next.new .next
      echo "build id: $(cat .next/BUILD_ID)"
      systemctl restart hive-ui
      sleep 2
      systemctl is-active hive-ui
      curl -fsS -o /dev/null -w "local :3002 status=%{http_code}\n" http://127.0.0.1:3002/ || true
    '; then
    echo "==> $host: FAILED (extract/restart)"; failed+=("$host"); echo; continue
  fi
  got="$(ssh "${ssh_opts[@]}" "root@$host" 'cat /root/hive/ui/.next/BUILD_ID' 2>/dev/null || echo none)"
  if [ "$got" != "$CANON_BUILD_ID" ]; then
    echo "==> $host: FAILED (BUILD_ID $got != canonical $CANON_BUILD_ID)"; failed+=("$host"); echo; continue
  fi
  echo "==> $host: done ($got)"
  echo
done
rm -f "$ARTIFACT"

if [ ${#failed[@]} -gt 0 ]; then
  echo "DEPLOY INCOMPLETE — ${#failed[@]} host(s) failed: ${failed[*]}" >&2
  exit 1
fi
echo "All ${#HOSTS[@]} host(s) deployed on canonical build $CANON_BUILD_ID."
