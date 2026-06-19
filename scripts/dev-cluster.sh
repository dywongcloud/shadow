#!/usr/bin/env bash
# Local development cluster: runs at least TWO OpenEdge P2P nodes, meshed
# together, so the multi-node behaviors are exercised by default — regions
# derived from each node's real geolocation, anycast routing, container leases,
# cross-node mesh routing and cluster resource totals.
#
# Each node gets its own data dir and its own DNS/TLS ports so they don't clash
# on a single machine. The UI talks to node-a's admin API on :8786.
#
# Usage:  ./scripts/dev-cluster.sh           # build + run node-a and node-b
#         NODES=3 ./scripts/dev-cluster.sh   # run 3 nodes (a, b, c)
#
# Stop everything with Ctrl-C.
set -euo pipefail
cd "$(dirname "$0")/.."

NODES="${NODES:-2}"
if [ "$NODES" -lt 2 ]; then NODES=2; fi   # local dev always runs >= 2 nodes
LOG_DIR="${TMPDIR:-/tmp}"
RUST_LOG="${RUST_LOG:-info,iroh=warn,guardian_db=warn,iroh_docs=warn,iroh_blobs=warn,iroh_gossip=warn}"
export RUST_LOG

echo "Building hive-cloud…"
cargo build -p hive-cloud
BIN=./target/debug/hive-cloud

pids=()
cleanup() { echo; echo "stopping cluster…"; for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup INT TERM EXIT

# node-a — primary; the dashboard proxies to its admin API on :8786.
HIVE_DATA="$HOME/.hive-cloud" \
  "$BIN" --name node-a --listen 127.0.0.1:8787 --admin 127.0.0.1:8786 \
  > "$LOG_DIR/node-a.log" 2>&1 &
pids+=("$!")
echo "node-a  pid $!  public :8787  admin :8786  dns :5354  tls :8443  log $LOG_DIR/node-a.log"

# node-b … node-N — peers. Each gets its own data dir + DNS/TLS ports, and meshes
# with node-a via --peer (announce is bidirectional, so node-a learns them too).
letter=( b c d e f g h )
for i in $(seq 2 "$NODES"); do
  idx=$((i - 2))
  name="node-${letter[$idx]}"
  pub=$((8787 + (i - 1) * 2))      # 8789, 8791, …
  adm=$((8786 + (i - 1) * 2))      # 8788, 8790, …
  dns=$((5354 + (i - 1)))          # 5355, 5356, …
  tls=$((8443 + (i - 1)))          # 8444, 8445, …
  HIVE_DATA="$HOME/.hive-cloud-${letter[$idx]}" \
  HIVE_DNS_ADDR="127.0.0.1:${dns}" \
  HIVE_TLS_ADDR="127.0.0.1:${tls}" \
    "$BIN" --name "$name" --listen "127.0.0.1:${pub}" --admin "127.0.0.1:${adm}" \
    --peer http://127.0.0.1:8786 \
    > "$LOG_DIR/${name}.log" 2>&1 &
  pids+=("$!")
  echo "${name}  pid $!  public :${pub}  admin :${adm}  dns :${dns}  tls :${tls}  log $LOG_DIR/${name}.log"
done

echo
echo "cluster up with ${NODES} nodes — UI → http://127.0.0.1:3002 (proxies node-a). Ctrl-C to stop."
wait
