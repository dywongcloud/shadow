#!/usr/bin/env bash
# shadwd — run the whole shadw platform as macOS launchd agents: dev.shadw.lima (the Lima
# nested-virt VM hosting REAL Firecracker microVMs), dev.shadw.node-a/node-b (hive-cloud nodes,
# mock cell backend since macOS has no /dev/kvm), dev.shadw.fc (in-VM Firecracker node, FC=1),
# dev.shadw.watchdog. Logs land in ~/Library/Logs/shadw/.
#
# Usage:
#   ./scripts/shadwd.sh install      # build (release+zkauth) + install + start all
#   ./scripts/shadwd.sh start|stop|restart|status|logs
#   ./scripts/shadwd.sh uninstall
#   ./scripts/shadwd.sh fc-daemon    # (experimental) in-VM Firecracker node via systemd
#
# Env: NODES=<n> (default 2), HIVE_DASHBOARD_URL (default http://localhost:3002)
set -uo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"
PROFILE="${PROFILE:-debug}"
BIN="$REPO/target/$PROFILE/hive-cloud"
LA="$HOME/Library/LaunchAgents"
LOGS="$HOME/Library/Logs/shadw"
DOMAIN="gui/$(id -u)"
NODES="${NODES:-2}"
FC="${FC:-1}"
DASH="${HIVE_DASHBOARD_URL:-http://localhost:3002}"
PUB_DASH="${HIVE_PUBLIC_DASHBOARD_URL:-https://shadw.cloud}"
BKK_PEER="${BKK_PEER:-http://127.0.0.1:18796}"
VA_PEER="${VA_PEER:-http://127.0.0.1:18797}"
SJ_PEER="${SJ_PEER:-http://127.0.0.1:18798}"
VA2_PEER="${VA2_PEER:-http://127.0.0.1:18799}"
NODE_BIN="$(dirname "$(command -v node 2>/dev/null || echo /usr/bin/false)")"
PATHVAL="$NODE_BIN:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/sbin:/usr/sbin"
RL="info,iroh=warn,guardian_db=warn,iroh_docs=warn,iroh_blobs=warn,iroh_gossip=warn"
WATCHDOG_LABEL=dev.shadw.watchdog
LABELS=(dev.shadw.lima dev.shadw.node-a)
[ "$NODES" -ge 2 ] && LABELS+=(dev.shadw.node-b)
[ "$FC" = 1 ] && LABELS+=(dev.shadw.fc)
LABELS+=("$WATCHDOG_LABEL")

xml_args() { for a in "$@"; do printf '    <string>%s</string>\n' "$a"; done; }

write_node_plist() {
  local label="$1" name="$2" pub="$3" adm="$4" dns="$5" tls="$6" data="$7"; shift 7
  local args=("$BIN" --name "$name" --listen "127.0.0.1:$pub" --admin "127.0.0.1:$adm")
  local peer
  for peer in "$@"; do [ -n "$peer" ] && args+=(--peer "$peer"); done
  cat > "$LA/$label.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array>
$(xml_args "${args[@]}")  </array>
  <key>EnvironmentVariables</key><dict>
    <key>HOME</key><string>$HOME</string>
    <key>PATH</key><string>$PATHVAL</string>
    <key>RUST_LOG</key><string>$RL</string>
    <key>HIVE_DATA</key><string>$data</string>
    <key>HIVE_DASHBOARD_URL</key><string>$DASH</string>
    <key>HIVE_PUBLIC_DASHBOARD_URL</key><string>$PUB_DASH</string>
    <key>HIVE_DNS_ADDR</key><string>127.0.0.1:$dns</string>
    <key>HIVE_TLS_ADDR</key><string>127.0.0.1:$tls</string>
  </dict>
  <key>WorkingDirectory</key><string>$REPO</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>$LOGS/$name.log</string>
  <key>StandardErrorPath</key><string>$LOGS/$name.log</string>
</dict></plist>
PLIST
}

write_lima_plist() {
  cat > "$LA/dev.shadw.lima.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.shadw.lima</string>
  <key>ProgramArguments</key><array>
    <string>/bin/bash</string>
    <string>$REPO/scripts/shadw-lima-up.sh</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>HOME</key><string>$HOME</string>
    <key>PATH</key><string>$PATHVAL</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>StartInterval</key><integer>300</integer>
  <key>StandardOutPath</key><string>$LOGS/lima.log</string>
  <key>StandardErrorPath</key><string>$LOGS/lima.log</string>
</dict></plist>
PLIST
}

write_watchdog_plist() {
  cat > "$LA/dev.shadw.watchdog.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.shadw.watchdog</string>
  <key>ProgramArguments</key><array>
    <string>/bin/bash</string>
    <string>$REPO/scripts/shadw-watchdog.sh</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>HOME</key><string>$HOME</string>
    <key>PATH</key><string>$PATHVAL</string>
    <key>WATCHDOG_LOOP</key><string>1</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$LOGS/watchdog.log</string>
  <key>StandardErrorPath</key><string>$LOGS/watchdog.log</string>
</dict></plist>
PLIST
}

write_fc_plist() {
  cat > "$LA/dev.shadw.fc.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.shadw.fc</string>
  <key>ProgramArguments</key><array>
    <string>/bin/bash</string>
    <string>$REPO/scripts/shadw-fc-up.sh</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>HOME</key><string>$HOME</string>
    <key>PATH</key><string>$PATHVAL</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>StartInterval</key><integer>60</integer>
  <key>StandardOutPath</key><string>$LOGS/fc.log</string>
  <key>StandardErrorPath</key><string>$LOGS/fc.log</string>
</dict></plist>
PLIST
}

load_agent()   { launchctl bootout "$DOMAIN/$1" 2>/dev/null; launchctl bootstrap "$DOMAIN" "$LA/$1.plist" 2>/dev/null; launchctl kickstart "$DOMAIN/$1" 2>/dev/null || true; }
unload_agent() { launchctl bootout "$DOMAIN/$1" 2>/dev/null || true; }

cmd_install() {
  echo "==> Building hive-cloud ($PROFILE, --features zkauth)…"
  if [ "$PROFILE" = "release" ]; then cargo build --release -p hive-cloud --features zkauth || { echo "build failed"; exit 1; }
  else cargo build -p hive-cloud --features zkauth || { echo "build failed"; exit 1; }; fi
  mkdir -p "$LA" "$LOGS"
  echo "==> Stopping any hand-started nodes (frees :8786/:8787)…"
  pkill -f "name node-[a-z] --listen" 2>/dev/null; sleep 1
  echo "==> Pruning stopped/leaked host containers (clears warm-up pressure)…"
  command -v podman >/dev/null 2>&1 && podman container prune -f >/dev/null 2>&1 || true
  echo "==> Writing LaunchAgents → $LA"
  write_lima_plist
  write_node_plist dev.shadw.node-a node-a 8787 8786 5354 8443 "$HOME/.hive-cloud" "$BKK_PEER" "$VA_PEER" "$SJ_PEER" "$VA2_PEER"
  [ "$NODES" -ge 2 ] && write_node_plist dev.shadw.node-b node-b 8789 8788 5355 8444 "$HOME/.hive-cloud-b" "http://127.0.0.1:8786"
  [ "$FC" = 1 ] && write_fc_plist
  write_watchdog_plist
  cmd_start
  echo "==> Installed. UI → http://localhost:3002 (proxies node-a :8786). Logs: $LOGS"
}

cmd_start()  { for l in "${LABELS[@]}"; do echo "  load $l"; load_agent "$l"; done; }
reap_leaked_mock_cells() { pkill -f "server.py" 2>/dev/null; pkill -f "hive-cells" 2>/dev/null; }
cmd_stop()   {
  unload_agent "$WATCHDOG_LABEL"
  for l in "${LABELS[@]}"; do [ "$l" = "$WATCHDOG_LABEL" ] && continue; echo "  unload $l"; unload_agent "$l"; done
  reap_leaked_mock_cells
  echo "stopped"
}
cmd_restart() { for l in "${LABELS[@]}"; do launchctl kickstart -k "$DOMAIN/$l" 2>/dev/null || load_agent "$l"; done; echo "restarted"; }
cmd_uninstall() { cmd_stop; for l in "${LABELS[@]}"; do rm -f "$LA/$l.plist"; done; echo "uninstalled (plists removed)"; }

cmd_status() {
  echo "=== launchd agents ==="
  for l in "${LABELS[@]}"; do
    if launchctl print "$DOMAIN/$l" >/dev/null 2>&1; then
      local pid st
      pid=$(launchctl print "$DOMAIN/$l" 2>/dev/null | awk -F'= ' '/^[[:space:]]*pid =/{print $2; exit}')
      st=$(launchctl print "$DOMAIN/$l" 2>/dev/null | awk -F'= ' '/last exit code/{print $2; exit}')
      printf "  %-18s loaded  pid=%-8s last_exit=%s\n" "$l" "${pid:-—}" "${st:-—}"
    else printf "  %-18s NOT loaded\n" "$l"; fi
  done
  echo "=== health ==="
  for p in 8786 8788; do printf "  node admin :%s -> " "$p"; curl -fsS -m2 "http://127.0.0.1:$p/healthz" 2>/dev/null && echo || echo DOWN; done
  printf "  public :8787 -> "; curl -s -m2 -o /dev/null -w "%{http_code}\n" -H "Host: x.localhost" http://127.0.0.1:8787/ 2>/dev/null || echo DOWN
  printf "  lima vm   -> "; limactl list --format '{{.Status}}' "${FC_VM:-hive}" 2>/dev/null || echo "absent"
  if [ "$FC" = 1 ]; then printf "  fc node :8796 -> "; curl -fsS -m2 "http://127.0.0.1:8796/healthz" 2>/dev/null && echo || echo "down (run: $0 fc-daemon)"; fi
}

cmd_logs() { echo "tailing $LOGS/*.log (Ctrl-C to stop)"; tail -n 30 -f "$LOGS"/*.log; }

cmd_fc_daemon() {
  local VM="${FC_VM:-hive}"
  local REPO_IN_VM="/Users/$(id -un)/fluid/hive"
  limactl list --format '{{.Status}}' "$VM" 2>/dev/null | grep -q Running || { echo "Lima VM '$VM' not running (run: $0 start)"; exit 1; }
  echo "==> [fc] resolving Cargo.lock on host (VM mount is read-only)…"
  cargo check -p hive-cell-agent >/dev/null 2>&1 || true
  echo "==> [fc] provisioning the VM: node runtime, static agent, guest rootfs, node binary, systemd unit"
  limactl shell "$VM" bash -lc '
    set -e
    REPO="'"$REPO_IN_VM"'"
    cd "$REPO" 2>/dev/null || cd "$HOME"
    export CARGO_TARGET_DIR=$HOME/fc-target

    command -v node >/dev/null 2>&1 || { sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq && sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nodejs npm; }
    sudo git config --system --add safe.directory "*" 2>/dev/null || true

    rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1 || true
    cargo build --release --target aarch64-unknown-linux-musl -p hive-cell-agent --locked
    sudo install -D -m0755 $HOME/fc-target/aarch64-unknown-linux-musl/release/hive-cell-agent /usr/local/bin/hive-cell-agent

    if [ ! -f /var/lib/hive/rootfs/default.ext4 ] || [ -n "${FC_REBUILD_ROOTFS:-}" ]; then
      sudo ROOTFS_DIR=/var/lib/hive/rootfs AGENT_BIN=/usr/local/bin/hive-cell-agent bash "$REPO/scripts/build-rootfs.sh" node:20-slim default 2048
    fi

    cargo build --release -p hive-cloud --features zkauth --locked
    BIN=$HOME/fc-target/release/hive-cloud
    sudo tee /etc/systemd/system/hive-fc.service >/dev/null <<UNIT
[Unit]
Description=shadw hive-cloud node (Firecracker backend)
After=network.target
[Service]
Environment=RUST_LOG=info
Environment=HIVE_DATA=/var/lib/hive/data
ExecStart=$BIN --name fc-node --listen 0.0.0.0:8797 --admin 0.0.0.0:8796
Restart=always
RestartSec=3
[Install]
WantedBy=multi-user.target
UNIT
    sudo mkdir -p /var/lib/hive/data
    sudo systemctl daemon-reload
    sudo systemctl enable --now hive-fc.service
    sleep 2; systemctl --no-pager --full status hive-fc.service | head -6
  '
  echo "==> in-VM Firecracker node installed. It provisions REAL microVMs (/dev/kvm)"
  echo "    and serves functions end-to-end. Listens in-VM on :8796/:8797"
  echo "    (Lima-forwarded to host 127.0.0.1); the dev.shadw.fc agent keeps it up."
}

case "${1:-}" in
  install)   cmd_install ;;
  start)     cmd_start ;;
  stop)      cmd_stop ;;
  restart)   cmd_restart ;;
  uninstall) cmd_uninstall ;;
  status)    cmd_status ;;
  logs)      cmd_logs ;;
  fc-daemon) cmd_fc_daemon ;;
  *) echo "usage: $0 <install|start|stop|restart|uninstall|status|logs|fc-daemon>"; exit 1 ;;
esac
