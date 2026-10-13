#!/usr/bin/env bash
# Fleet-wide rollout of the ui/ dashboard (hive-ui.service, next-server on 127.0.0.1:3002).
# PREFERRED PATH: ansible-playbook ansible/playbooks/parallel-deploy.yml --tags ui; this script
# is the serial fallback. A backend roll never ships ui/ changes. ONE build (first host) is
# distributed byte-identically: Next builds are not deterministic, so per-node builds behind one
# round-robin hostname 404 each other's chunks.
#
# Usage: scripts/deploy-ui-fleet.sh [host ...]   (defaults to the fleet inventory)
set -euo pipefail

PEM="${HIVE_FLEET_PEM:-$HOME/Documents/billing.pem}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Hosts derive from ansible/inventory/hosts.ini: the control-plane leader (first entry of
# hive_cp_owner_chain) first, then inventory order.
INVENTORY="$REPO_ROOT/ansible/inventory/hosts.ini"
[ -f "$INVENTORY" ] || { echo "inventory not found: $INVENTORY" >&2; exit 1; }

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

# One multiplexed connection per host: rapid separate connections trip sshd's MaxStartups
# (10:30:100) while background automation holds its own.
CTL_DIR="$(mktemp -d)"
ssh_opts=(-i "$PEM" -o StrictHostKeyChecking=no -o ConnectTimeout=8 \
  -o ControlMaster=auto -o "ControlPath=$CTL_DIR/%r@%h" -o ControlPersist=120)
trap 'rm -rf "$CTL_DIR"' EXIT

# Continue past a failed host, but exit non-zero at the end.
failed=()

ARTIFACT="$(mktemp -d)/next-build.tar.gz"
ARTIFACT_WF="$(mktemp -d)/wfconsole-build.tar.gz"
BUILD_HOST=""
CANON_BUILD_ID=""
CANON_WF_MD5=""

# The patched @workflow/web console build is loaded from node_modules at request time (rsync
# excludes node_modules; the patch runs only inside `npm run build` on the build host), so it
# ships as a second artifact, overlaid onto every follower and marker-verified there.
wf_console_package() {
  # $1 = remote command prefix host. Packages + marker-verifies on the build host.
  ssh "${ssh_opts[@]}" "root@$1" '
    set -e
    cd /root/hive/ui
    grep -rl "__hive_rpc_guard" node_modules/@workflow/web/build/server/assets/ >/dev/null \
      || { echo "WF CONSOLE PATCH MISSING on build host (patch-wf-console.mjs did not land)"; exit 1; }
    tar -C node_modules/@workflow/web -czf /root/hive/ui/wfconsole-artifact.tar.gz build
    md5sum node_modules/@workflow/web/build/server/assets/server-build-*.js | md5sum | cut -d" " -f1
  '
}

wf_console_overlay() {
  # $1 = follower host. Overlay the shipped build onto the pristine npm-ci tree; verify the marker.
  ssh "${ssh_opts[@]}" "root@$1" '
    set -e
    cd /root/hive/ui
    tar -C node_modules/@workflow/web -xzf wfconsole-artifact.tar.gz
    rm -f wfconsole-artifact.tar.gz
    grep -rl "__hive_rpc_guard" node_modules/@workflow/web/build/server/assets/ >/dev/null \
      || { echo "WF CONSOLE PATCH MISSING after overlay"; exit 1; }
    md5sum node_modules/@workflow/web/build/server/assets/server-build-*.js | md5sum | cut -d" " -f1
  '
}

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
  ssh "${ssh_opts[@]}" "root@$host" true 2>/dev/null || sleep 5
  ssh "${ssh_opts[@]}" "root@$host" '
    command -v node >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: nodejs not installed"; exit 1; }
    command -v npm  >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: npm not installed"; exit 1; }
    # The @workflow/web 4.1.13 console build needs node:sqlite (Node >= 22.5); a v20 node serves /wfc as 500.
    node -e "require(\"node:sqlite\")" 2>/dev/null \
      || { echo "PREFLIGHT FAIL: node $(node --version) lacks node:sqlite (need >= 22.5)"; exit 1; }
    [ -f /etc/systemd/system/hive-ui.service ] || { echo "PREFLIGHT FAIL: hive-ui.service unit missing"; exit 1; }
    echo "  node $(node --version), npm $(npm --version), unit present"
  '
}

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
      # The cache dir is node-local build state and huge; never ship it.
      tar -C .next --exclude cache -czf /root/hive/ui/.next-artifact.tar.gz .
      systemctl restart hive-ui
      sleep 2
      systemctl is-active hive-ui
      curl -fsS -o /dev/null -w "local :3002 status=%{http_code}\n" http://127.0.0.1:3002/ || true
    '; then
    BUILD_HOST="$host"
    CANON_BUILD_ID="$(ssh "${ssh_opts[@]}" "root@$host" 'cat /root/hive/ui/.next/BUILD_ID')"
    echo "==> $host: canonical build $CANON_BUILD_ID; pulling artifacts"
    CANON_WF_MD5="$(wf_console_package "$host")"
    echo "==> $host: canonical wf-console build $CANON_WF_MD5"
    scp -q "${ssh_opts[@]}" "root@$host:/root/hive/ui/.next-artifact.tar.gz" "$ARTIFACT"
    scp -q "${ssh_opts[@]}" "root@$host:/root/hive/ui/wfconsole-artifact.tar.gz" "$ARTIFACT_WF"
    ssh "${ssh_opts[@]}" "root@$host" 'rm -f /root/hive/ui/.next-artifact.tar.gz /root/hive/ui/wfconsole-artifact.tar.gz'
    break
  else
    echo "==> $host: FAILED to build; trying the next host as build host"
    failed+=("$host")
  fi
done
[ -n "$BUILD_HOST" ] || { echo "FATAL: no host could produce the canonical build" >&2; exit 1; }

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
  echo "==> $host: pushing canonical artifacts ($CANON_BUILD_ID, wf-console $CANON_WF_MD5)"
  if ! scp -q "${ssh_opts[@]}" "$ARTIFACT" "root@$host:/root/hive/ui/.next-artifact.tar.gz"; then
    echo "==> $host: FAILED (artifact push)"; failed+=("$host"); echo; continue
  fi
  if ! scp -q "${ssh_opts[@]}" "$ARTIFACT_WF" "root@$host:/root/hive/ui/wfconsole-artifact.tar.gz"; then
    echo "==> $host: FAILED (wf-console artifact push)"; failed+=("$host"); echo; continue
  fi
  if ! ssh "${ssh_opts[@]}" "root@$host" '
      set -e
      cd /root/hive/ui
      npm ci
      rm -rf .next.new && mkdir .next.new
      tar -C .next.new -xzf .next-artifact.tar.gz
      rm -f .next-artifact.tar.gz
      rm -rf .next.old
      [ -d .next ] && mv .next .next.old
      mv .next.new .next
      echo "build id: $(cat .next/BUILD_ID)"
      # The console overlay is loaded at request time, so it must land before the restart.
      tar -C node_modules/@workflow/web -xzf wfconsole-artifact.tar.gz
      rm -f wfconsole-artifact.tar.gz
      grep -rl "__hive_rpc_guard" node_modules/@workflow/web/build/server/assets/ >/dev/null \
        || { echo "WF CONSOLE PATCH MISSING after overlay"; exit 1; }
      md5sum node_modules/@workflow/web/build/server/assets/server-build-*.js | md5sum | cut -d" " -f1
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
  got_wf="$(ssh "${ssh_opts[@]}" "root@$host" 'cd /root/hive/ui && md5sum node_modules/@workflow/web/build/server/assets/server-build-*.js | md5sum | cut -d" " -f1' 2>/dev/null || echo none)"
  if [ "$got_wf" != "$CANON_WF_MD5" ]; then
    echo "==> $host: FAILED (wf-console md5 $got_wf != canonical $CANON_WF_MD5)"; failed+=("$host"); echo; continue
  fi
  echo "==> $host: done ($got, wf-console $got_wf)"
  echo
done
rm -f "$ARTIFACT"

if [ ${#failed[@]} -gt 0 ]; then
  echo "DEPLOY INCOMPLETE — ${#failed[@]} host(s) failed: ${failed[*]}" >&2
  exit 1
fi
echo "All ${#HOSTS[@]} host(s) deployed on canonical build $CANON_BUILD_ID."
