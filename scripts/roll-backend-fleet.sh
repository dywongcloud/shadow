#!/bin/bash
# Roll the hive-cloud backend binary fleet-wide with NODE-TO-NODE distribution.
#
# The fleet has two glibc groups needing separate native builds (see AGENTS.md
# "Fleet has two glibc groups"): 2.39 builds on va, 2.38 builds on bkk. Each
# build node then pushes its binary DIRECTLY to its group's peers over the
# datacenter links (1-65ms RTTs), using SSH agent forwarding for auth — the
# operator laptop only orchestrates and verifies. Pushing 13x ~100MB through a
# residential uplink is what this replaces: it took minutes per node and
# long-haul transfers (hongkong) reset mid-stream.
#
# Requires: ssh-agent holding the fleet key (ssh-add ~/Documents/billing.pem).
# Usage: roll-backend-fleet.sh [--build]   (--build = rsync crates/ + rebuild first)
set -u
KEY=~/Documents/billing.pem
SSHO=(-i "$KEY" -o StrictHostKeyChecking=no -o ConnectTimeout=15)
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Build sources per glibc group. Group membership is by OS IMAGE, not region:
# 2.38 = bkk, hk, and ALL FIVE GPU/CVM San Jose nodes (TencentOS); 2.39 = the
# Rocky nodes (va, va2, va3, sj, sj2, sao-paulo, frankfurt).
#
# Do NOT hand-maintain these lists from memory: scripts/audit-runtime-versions.sh
# prints every node's live glibc and is the check to run before ANY roll — it is
# how frankfurt was found missing from TGT239 (2026-07-31, silently never rolled).
SRC239=43.166.206.175  # va
SRC238=43.152.247.70   # bkk
TGT239="170.106.158.151 170.106.40.67 43.172.25.45 43.173.78.95 43.166.76.159 162.62.83.144"
TGT238="43.128.46.225 43.166.223.197 43.166.233.114 43.153.106.173 170.106.155.130 162.62.83.91"

BIN_PATH=/root/hive/target/release/hive-cloud

build() { # $1 src ip
  rsync -az --delete -e "ssh ${SSHO[*]}" "$REPO_ROOT/crates/" "root@$1:/root/hive/crates/" || return 1
  # rsync preserves local mtimes; touch so cargo cannot silently skip the rebuild.
  ssh "${SSHO[@]}" "root@$1" 'cd /root/hive && find crates -name "*.rs" -exec touch {} + && cargo build --release -p hive-cloud 2>&1 | tail -1'
}

swap_restart() { # $1 target ip, $2 expected sha256
  ssh "${SSHO[@]}" "root@$1" "
    set -e
    got=\$(sha256sum /tmp/hive-cloud.new | cut -d' ' -f1)
    [ \"\$got\" = \"$2\" ] || { echo SHA_MISMATCH; exit 1; }
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

roll_group() { # $1 src ip, $2 targets
  local src=$1 targets=$2
  local sha
  sha=$(ssh "${SSHO[@]}" "root@$src" "sha256sum $BIN_PATH | cut -d' ' -f1") || { echo "$src SHA_READ_FAIL"; return 1; }
  echo "group source $src sha=${sha:0:12}"
  for tgt in $targets; do
    # Node-to-node push: agent forwarding (-A) lets the source node
    # authenticate to the target with the operator's key, never stored on it.
    # BatchMode on the inner hop is load-bearing: without it, a missing
    # forwarded agent degrades to a password prompt that hangs forever
    # silently instead of failing fast.
    if ssh -A "${SSHO[@]}" "root@$src" "scp -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=15 $BIN_PATH root@$tgt:/tmp/hive-cloud.new" >/dev/null 2>&1 \
       || ssh -A "${SSHO[@]}" "root@$src" "scp -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=15 $BIN_PATH root@$tgt:/tmp/hive-cloud.new" >/dev/null 2>&1; then
      if swap_restart "$tgt" "$sha" | grep -q GATE_OK; then echo "$tgt ROLLED_OK"; else echo "$tgt GATE_FAIL"; fi
    else
      echo "$tgt TRANSFER_FAIL"
    fi
  done
  # The source node itself swaps from its own local build (no transfer).
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
