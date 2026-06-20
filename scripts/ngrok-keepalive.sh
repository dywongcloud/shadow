#!/usr/bin/env bash
# Keep the ngrok tunnels (dashboard + deployment wildcard) always up. ngrok's
# agent can exit on a network blip or session limit; this restarts it
# automatically so shadow.ngrok.pizza never silently goes offline.
#
#   dashboard → shadow.ngrok.pizza            → :3002 (UI)
#   gateway   → *.deployment.shadow.ngrok.pizza → :8787 (deployments)
#
# Usage:   ./scripts/ngrok-keepalive.sh            # foreground (Ctrl-C stops)
#          nohup ./scripts/ngrok-keepalive.sh &    # background, survives the shell
set -u
cd "$(dirname "$0")/.."

DEFAULT_CFG="$HOME/Library/Application Support/ngrok/ngrok.yml"
REPO_CFG="$(pwd)/ngrok.yml"
LOG="${TMPDIR:-/tmp}/ngrok.log"
RESTART_DELAY="${RESTART_DELAY:-3}"

child=""
cleanup() { [ -n "$child" ] && kill "$child" 2>/dev/null; exit 0; }
trap cleanup INT TERM EXIT

# Never run two agents — they'd clash on the reserved hostnames.
pkill -f "ngrok start" 2>/dev/null
sleep 1

echo "ngrok keep-alive starting — agent log: $LOG"
while true; do
  ngrok start --all \
    --config "$DEFAULT_CFG" \
    --config "$REPO_CFG" \
    --log=stdout --log-format=logfmt >> "$LOG" 2>&1 &
  child=$!
  echo "$(date '+%Y-%m-%d %H:%M:%S') ngrok up (pid $child)"
  wait "$child"
  code=$?
  child=""
  echo "$(date '+%Y-%m-%d %H:%M:%S') ngrok exited (code $code) — restarting in ${RESTART_DELAY}s"
  sleep "$RESTART_DELAY"
done
