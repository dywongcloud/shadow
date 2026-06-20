#!/usr/bin/env bash
# Local development cluster: runs at least TWO OpenEdge P2P nodes, meshed
# together, so the multi-node behaviors are exercised by default — regions
# derived from each node's real geolocation, anycast routing, container leases,
# cross-node mesh routing and cluster resource totals.
#
# Each node gets its own data dir and its own DNS/TLS ports so they don't clash
# on a single machine. The UI talks to node-a's admin API on :8786.
#
# IDEMPOTENT: if a node is ALREADY running on its admin port (e.g. you started
# node-a by hand earlier), it is left alone and only the MISSING nodes are
# started. This invocation only ever manages (and stops on Ctrl-C) the nodes it
# actually launched — pre-existing nodes keep running after you exit.
#
# Usage:  ./scripts/dev-cluster.sh           # ensure node-a and node-b are up
#         NODES=3 ./scripts/dev-cluster.sh   # ensure 3 nodes (a, b, c)
#
# Stop the nodes THIS run started with Ctrl-C.
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
started=0
reused=0
cleanup() {
  # Only stop the nodes THIS invocation started; leave pre-existing ones running.
  if [ "${#pids[@]}" -gt 0 ]; then
    echo; echo "stopping the ${#pids[@]} node(s) this run started…"
    for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  fi
}
trap cleanup INT TERM EXIT

# Is a healthy hive-cloud node already answering on this admin port?
node_running() { curl -fsS -m1 "http://127.0.0.1:$1/healthz" >/dev/null 2>&1; }

# start_node <name> <public> <admin> <dns> <tls> <data-dir> [extra flags…]
# Reuses an already-running node on <admin>; otherwise launches it in the
# background and records its pid so cleanup can stop it.
start_node() {
  local name="$1" pub="$2" adm="$3" dns="$4" tls="$5" data="$6"; shift 6
  if node_running "$adm"; then
    echo "${name}  already running on admin :${adm} — reusing (left running on exit)"
    reused=$((reused + 1))
    return 0
  fi
  HIVE_DATA="$data" \
  HIVE_DNS_ADDR="127.0.0.1:${dns}" \
  HIVE_TLS_ADDR="127.0.0.1:${tls}" \
    "$BIN" --name "$name" --listen "127.0.0.1:${pub}" --admin "127.0.0.1:${adm}" "$@" \
    > "$LOG_DIR/${name}.log" 2>&1 &
  local pid=$!
  pids+=("$pid")
  started=$((started + 1))
  echo "${name}  pid ${pid}  public :${pub}  admin :${adm}  dns :${dns}  tls :${tls}  log $LOG_DIR/${name}.log"
}

# node-a — primary; the dashboard proxies to its admin API on :8786. No --peer:
# peers announce themselves to it (announce is bidirectional).
start_node node-a 8787 8786 5354 8443 "$HOME/.hive-cloud"

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
  start_node "$name" "$pub" "$adm" "$dns" "$tls" "$HOME/.hive-cloud-${letter[$idx]}" \
    --peer http://127.0.0.1:8786
done

echo
if [ "$started" -eq 0 ]; then
  echo "all ${NODES} nodes already running (${reused} reused) — nothing to start. UI → http://127.0.0.1:3002"
  trap - INT TERM EXIT   # nothing of ours to clean up; don't touch the running nodes
  exit 0
fi

echo "cluster up — ${started} started, ${reused} reused (${NODES} total). UI → http://127.0.0.1:3002 (proxies node-a)."
echo "Ctrl-C stops the ${started} node(s) this run started."
wait
