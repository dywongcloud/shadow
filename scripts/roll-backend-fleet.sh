#!/bin/bash
# Roll the hive-cloud backend binary fleet-wide with NODE-TO-NODE distribution: each glibc
# group's build node pushes its binary directly to its peers over datacenter links (agent
# forwarding for auth); the operator laptop only orchestrates. PREFERRED PATH:
# ansible-playbook ansible/playbooks/parallel-deploy.yml --tags backend; this is the fallback.
#
# Requires: ssh-agent holding the fleet key (ssh-add ~/Documents/billing.pem).
# Usage: roll-backend-fleet.sh [--build]   (--build = rsync crates/ + rebuild first)
set -u
KEY=~/Documents/billing.pem
SSHO=(-i "$KEY" -o StrictHostKeyChecking=no -o ConnectTimeout=15)
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Build sources per glibc group (membership is by OS image, never region); run
# scripts/audit-runtime-versions.sh before any roll instead of maintaining these from memory.
SRC239=43.166.206.175
SRC238=43.152.247.70
TGT239="170.106.158.151 170.106.40.67 43.172.25.45 43.173.78.95 43.166.76.159 162.62.83.144"
TGT238="43.128.46.225 43.166.223.197 43.166.233.114 43.153.106.173 170.106.155.130 162.62.83.91"

BIN_PATH=/root/hive/target/release/hive-cloud

build() {
  local src_ip="$1"
  # The root manifest + lockfile carry workspace.members; crates/ alone is not a buildable tree.
  rsync -az --delete -e "ssh ${SSHO[*]}" "$REPO_ROOT/crates/" "root@$src_ip:/root/hive/crates/" || return 1
  rsync -az -e "ssh ${SSHO[*]}" "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/Cargo.lock" "root@$src_ip:/root/hive/" || return 1
  # touch defeats rsync's preserved mtimes so cargo cannot skip the rebuild; pipefail fails the roll.
  ssh "${SSHO[@]}" "root@$src_ip" 'set -o pipefail; cd /root/hive && find crates -name "*.rs" -exec touch {} + && cargo build --release -p hive-cloud 2>&1 | tail -1'
}

swap_restart() {
  local target_ip="$1" expected_sha="$2"
  ssh "${SSHO[@]}" "root@$target_ip" "
    set -e
    got=\$(sha256sum /tmp/hive-cloud.new | cut -d' ' -f1)
    [ \"\$got\" = \"$expected_sha\" ] || { echo SHA_MISMATCH; exit 1; }
    BIN=\$(systemctl show -p ExecStart --value hive-node | sed -n 's/.*path=\([^ ;]*\).*/\1/p')
    cp -f \"\$BIN\" \"\$BIN.old-roll\"
    chmod +x /tmp/hive-cloud.new
    mv -f /tmp/hive-cloud.new \"\$BIN\"
    systemctl restart hive-node
    sleep 10
    systemctl is-active hive-node >/dev/null
    code=\$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 http://127.0.0.1:8786/v1/nodes)
    case \"\$code\" in 200|401|403) echo GATE_OK;; *) echo GATE_FAIL_\$code; exit 1;; esac
  "
}

roll_group() {
  local src=$1 targets=$2
  local sha
  sha=$(ssh "${SSHO[@]}" "root@$src" "sha256sum $BIN_PATH | cut -d' ' -f1") || { echo "$src SHA_READ_FAIL"; return 1; }
  echo "group source $src sha=${sha:0:12}"
  for tgt in $targets; do
    # -A forwards the operator's agent; BatchMode on the inner hop fails fast instead of hanging on a
    # password prompt.
    if ssh -A "${SSHO[@]}" "root@$src" "scp -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=15 $BIN_PATH root@$tgt:/tmp/hive-cloud.new" >/dev/null 2>&1 \
       || ssh -A "${SSHO[@]}" "root@$src" "scp -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=15 $BIN_PATH root@$tgt:/tmp/hive-cloud.new" >/dev/null 2>&1; then
      if swap_restart "$tgt" "$sha" | grep -q GATE_OK; then echo "$tgt ROLLED_OK"; else echo "$tgt GATE_FAIL"; fi
    else
      echo "$tgt TRANSFER_FAIL"
    fi
  done
  ssh "${SSHO[@]}" "root@$src" "cp -f $BIN_PATH /tmp/hive-cloud.new" \
    && { if swap_restart "$src" "$sha" | grep -q GATE_OK; then echo "$src ROLLED_OK(src)"; else echo "$src GATE_FAIL(src)"; fi; }
}

if [ "${1:-}" = "--build" ]; then
  echo "== building 2.39 on $SRC239 and 2.38 on $SRC238 (parallel) =="
  build $SRC239 & P1=$!
  build $SRC238 & P2=$!
  wait $P1 || exit 1
  wait $P2 || exit 1
fi
roll_group $SRC239 "$TGT239"
roll_group $SRC238 "$TGT238"
echo "FLEET_ROLL_DONE"
