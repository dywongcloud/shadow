#!/usr/bin/env bash
# Watchdog for the shadw node daemons on macOS: restarts ONLY a node that failed EVERY probe
# across a sustained window (never on one slow /healthz). Runs as a persistent loop under the
# dev.shadw.watchdog LaunchAgent (KeepAlive + WATCHDOG_LOOP=1; StartInterval never fires in this
# launchd domain); without WATCHDOG_LOOP it runs one pass and exits.
set -uo pipefail
DOMAIN="gui/$(id -u)"

STRIKES=4
PROBE_TIMEOUT=8
SLEEP_BETWEEN=6
BOOTOUT_SETTLE_SECS=3

healthy() { curl -fsS -m"$PROBE_TIMEOUT" "http://127.0.0.1:$1/healthz" >/dev/null 2>&1; }

# /healthz stays green during silent partial-mesh isolation; `isolated:true` means peers were
# expected and none are visible. No expected peers or an unreachable endpoint counts as fine.
mesh_ok() {
  local body
  body=$(curl -fsS -m"$PROBE_TIMEOUT" "http://127.0.0.1:$1/v1/mesh" 2>/dev/null) || return 0
  case "$body" in
    *'"isolated":true'*) return 1 ;;
    *) return 0 ;;
  esac
}

# A DIFFERENT hive-cloud squatting the admin port passes /healthz and /v1/mesh; argv is
# deterministic, so no sustained window is needed. Prints the squatter pid, returns 0 only then.
imposter_pid() {
  local expected_name="$1" admin_port="$2" pid args
  pid=$(lsof -nP -tiTCP:"$admin_port" -sTCP:LISTEN 2>/dev/null | head -1)
  [ -n "$pid" ] || return 1
  args=$(ps -o command= -p "$pid" 2>/dev/null) || return 1
  case "$args" in
    *"--name $expected_name "*|*"--name $expected_name") return 1 ;;
    *hive-cloud*) echo "$pid"; return 0 ;;
    *) return 1 ;;
  esac
}

# launchd exit code 78 (EX_CONFIG, "Unable to get updated LWCR") means the registered job
# definition is stale: only bootout + bootstrap cures it. hive-cloud itself never exits 78.
spawn_broken() {
  local label="$1"
  launchctl print "$DOMAIN/$label" 2>/dev/null | grep -q 'last exit code = 78'
}

reload_job() {
  local label="$1"
  local plist="$HOME/Library/LaunchAgents/$label.plist"
  [ -f "$plist" ] || return 1
  launchctl bootout "$DOMAIN/$label" 2>/dev/null || true
  sleep "$BOOTOUT_SETTLE_SECS"
  launchctl bootstrap "$DOMAIN" "$plist" 2>/dev/null
}

job_registered() { launchctl print "$DOMAIN/$1" >/dev/null 2>&1; }

ensure() {
  local label="$1" admin_port="$2"
  job_registered "$label" || return 0

  # Evict a squatter before interpreting health: every probe below would interrogate the wrong process.
  local name pid ilabel
  name=${label#dev.shadw.}
  if pid=$(imposter_pid "$name" "$admin_port"); then
    ilabel=$(launchctl list 2>/dev/null | awk -v p="$pid" '$1 == p {print $3}')
    echo "$(date '+%H:%M:%S') $label IMPOSTOR on :$admin_port (pid $pid label ${ilabel:-none} is not --name $name) — evicting and restarting $label"
    if [ -n "$ilabel" ]; then
      launchctl bootout "$DOMAIN/$ilabel" 2>/dev/null || kill "$pid" 2>/dev/null || true
    else
      kill "$pid" 2>/dev/null || true
    fi
    sleep 2
    launchctl kickstart -k "$DOMAIN/$label" 2>/dev/null || true
    return 0
  fi

  if healthy "$admin_port"; then
    # Same sustained-window discipline: only STRIKES consecutive isolated reads earn a restart.
    mesh_ok "$admin_port" && return 0
    local j
    for ((j = 2; j <= STRIKES; j++)); do
      sleep "$SLEEP_BETWEEN"
      mesh_ok "$admin_port" && return 0
    done
    echo "$(date '+%H:%M:%S') $label ISOLATED on :$admin_port (/v1/mesh isolated=true across $STRIKES probes; /healthz green) — restarting to rejoin mesh"
    launchctl kickstart -k "$DOMAIN/$label" 2>/dev/null || true
    return 0
  fi

  local i
  for ((i = 2; i <= STRIKES; i++)); do
    sleep "$SLEEP_BETWEEN"
    healthy "$admin_port" && return 0
  done

  # Failed every probe across the window; `-k` clears a wedged process that will not exit by itself.
  echo "$(date '+%H:%M:%S') $label down on :$admin_port (failed $STRIKES probes over ~$((STRIKES * (PROBE_TIMEOUT + SLEEP_BETWEEN)))s) — restarting"
  if spawn_broken "$label"; then
    echo "$(date '+%H:%M:%S') $label cannot be spawned (last exit 78 EX_CONFIG) — kickstart cannot fix that; re-registering its plist"
    reload_job "$label" && return 0
  fi
  launchctl kickstart -k "$DOMAIN/$label" 2>/dev/null || true
}

run_checks() {
  ensure dev.shadw.fc-lax 8786
  ensure dev.shadw.fc-lax2 8788
}

if [ "${WATCHDOG_LOOP:-}" = "1" ]; then
  echo "$(date '+%H:%M:%S') watchdog loop up (pid $$, every 30s)"
  while true; do
    run_checks
    sleep 30
  done
else
  run_checks
fi
