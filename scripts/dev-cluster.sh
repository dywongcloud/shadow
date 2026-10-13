#!/usr/bin/env bash
# Local dev cluster: at least TWO meshed hive-cloud nodes, each with its own data dir and DNS/TLS ports. The UI proxies
# node-a's admin API (:8786). Idempotent: a node already answering on its admin port is reused and left running; Ctrl-C
# stops only the nodes THIS run started.
#
# Usage:  ./scripts/dev-cluster.sh           # ensure node-a and node-b are up
#         NODES=3 ./scripts/dev-cluster.sh   # ensure 3 nodes (a, b, c)
set -euo pipefail
cd "$(dirname "$0")/.."

MIN_NODES=2
NODES="${NODES:-$MIN_NODES}"
if [ "$NODES" -lt "$MIN_NODES" ]; then NODES="$MIN_NODES"; fi
PRIMARY_PUBLIC=8787; PRIMARY_ADMIN=8786; PRIMARY_DNS=5354; PRIMARY_TLS=8443
LOG_DIR="${TMPDIR:-/tmp}"
RUST_LOG="${RUST_LOG:-info,iroh=warn,guardian_db=warn,iroh_docs=warn,iroh_blobs=warn,iroh_gossip=warn}"
export RUST_LOG

echo "Building hive-cloud…"
cargo build -p hive-cloud
BIN=./target/debug/hive-cloud

started_pids=()
started=0
reused=0
cleanup() {
  if [ "${#started_pids[@]}" -gt 0 ]; then
    echo; echo "stopping the ${#started_pids[@]} node(s) this run started…"
    for p in "${started_pids[@]}"; do kill "$p" 2>/dev/null || true; done
  fi
}
trap cleanup INT TERM EXIT

node_running() { curl -fsS -m1 "http://127.0.0.1:$1/healthz" >/dev/null 2>&1; }

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
  started_pids+=("$pid")
  started=$((started + 1))
  echo "${name}  pid ${pid}  public :${pub}  admin :${adm}  dns :${dns}  tls :${tls}  log $LOG_DIR/${name}.log"
}

start_node node-a "$PRIMARY_PUBLIC" "$PRIMARY_ADMIN" "$PRIMARY_DNS" "$PRIMARY_TLS" "$HOME/.hive-cloud"

letter=( b c d e f g h )
for i in $(seq 2 "$NODES"); do
  idx=$((i - 2))
  name="node-${letter[$idx]}"
  pub=$((PRIMARY_PUBLIC + (i - 1) * 2))
  adm=$((PRIMARY_ADMIN + (i - 1) * 2))
  dns=$((PRIMARY_DNS + (i - 1)))
  tls=$((PRIMARY_TLS + (i - 1)))
  start_node "$name" "$pub" "$adm" "$dns" "$tls" "$HOME/.hive-cloud-${letter[$idx]}" \
    --peer "http://127.0.0.1:$PRIMARY_ADMIN"
done

echo
if [ "$started" -eq 0 ]; then
  echo "all ${NODES} nodes already running (${reused} reused) — nothing to start. UI → http://127.0.0.1:3002"
  trap - INT TERM EXIT
  exit 0
fi

echo "cluster up — ${started} started, ${reused} reused (${NODES} total). UI → http://127.0.0.1:3002 (proxies node-a)."
echo "Ctrl-C stops the ${started} node(s) this run started."
wait
