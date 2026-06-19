#!/usr/bin/env bash
# Start the public ngrok tunnels for Shadow with ONE command:
#   * dashboard  shadow.ngrok.pizza            -> :3002 (the UI)
#   * gateway    *.deployment.shadow.ngrok.pizza -> :8787 (live deployments)
#
# It merges your DEFAULT ngrok config (which holds your authtoken) with the
# repo's ngrok.yml, so the token stays out of the repo and you don't have to
# pass two --config flags by hand.
#
#   ./scripts/dev-tunnels.sh              # just the tunnels
#   ./scripts/dev-tunnels.sh --full       # also boot 2 nodes + the UI first
#
# One-time setup (fixes ERR_NGROK_318): reserve the wildcard domain
#   https://dashboard.ngrok.com/domains/new  ->  *.deployment.shadow.ngrok.pizza
set -euo pipefail
cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"
REPO_CFG="$REPO_ROOT/ngrok.yml"

# Locate the default ngrok config (holds the authtoken installed via
# `ngrok config add-authtoken`). Passing --config disables auto-loading it, so we
# pass it explicitly and merge.
default_cfg=""
for c in \
  "$HOME/Library/Application Support/ngrok/ngrok.yml" \
  "$HOME/.config/ngrok/ngrok.yml" \
  "$HOME/.ngrok2/ngrok.yml"; do
  [ -f "$c" ] && { default_cfg="$c"; break; }
done
if [ -z "$default_cfg" ]; then
  echo "✗ No default ngrok config found. Install your token first:" >&2
  echo "    ngrok config add-authtoken <token>   (https://dashboard.ngrok.com/get-started/your-authtoken)" >&2
  exit 1
fi
if [ ! -f "$REPO_CFG" ]; then
  echo "✗ Missing $REPO_CFG" >&2; exit 1
fi

# Optional: bring up the local cluster + dashboard first.
if [ "${1:-}" = "--full" ]; then
  RUST_LOG="${RUST_LOG:-info,iroh=warn,guardian_db=warn,iroh_docs=warn,iroh_blobs=warn,iroh_gossip=warn}"
  export RUST_LOG
  echo "→ building hive-cloud…"; cargo build -p hive-cloud
  BIN="$REPO_ROOT/target/debug/hive-cloud"; LOG_DIR="${TMPDIR:-/tmp}"
  if ! lsof -ti tcp:8786 -sTCP:LISTEN >/dev/null 2>&1; then
    HIVE_DATA="$HOME/.hive-cloud" "$BIN" --name node-a --listen 127.0.0.1:8787 --admin 127.0.0.1:8786 \
      > "$LOG_DIR/node-a.log" 2>&1 & echo "  node-a → :8787/:8786 ($LOG_DIR/node-a.log)"
  fi
  if ! lsof -ti tcp:8788 -sTCP:LISTEN >/dev/null 2>&1; then
    HIVE_DATA="$HOME/.hive-cloud-b" HIVE_DNS_ADDR=127.0.0.1:5355 HIVE_TLS_ADDR=127.0.0.1:8444 \
      "$BIN" --name node-b --listen 127.0.0.1:8789 --admin 127.0.0.1:8788 --peer http://127.0.0.1:8786 \
      > "$LOG_DIR/node-b.log" 2>&1 & echo "  node-b → :8789/:8788 ($LOG_DIR/node-b.log)"
  fi
  if ! lsof -ti tcp:3002 -sTCP:LISTEN >/dev/null 2>&1; then
    ( cd ui && npx next dev -p 3002 > "$LOG_DIR/ui-dev.log" 2>&1 & ) ; echo "  UI → :3002 ($LOG_DIR/ui-dev.log)"
  fi
  sleep 3
fi

echo "→ ngrok: shadow.ngrok.pizza (UI) + *.deployment.shadow.ngrok.pizza (deployments)"
echo "  authtoken config: $default_cfg"
echo "  tunnels config:   $REPO_CFG"
echo "  Ctrl-C to stop the tunnels."
exec ngrok start --all --config "$default_cfg" --config "$REPO_CFG"
