#!/usr/bin/env bash
# Watchdog for the shadw node daemons: ensures each installed node answers on its
# admin /healthz; restarts ONLY a node that is genuinely down/wedged. Runs as a
# PERSISTENT loop under the dev.shadw.watchdog LaunchAgent (KeepAlive daemon,
# WATCHDOG_LOOP=1 in the plist) — NOT StartInterval, which was live-witnessed
# silently never firing on this launchd domain (runs=0 across minutes even
# after a fresh bootout+bootstrap, while fc-lax2 sat crashed). A KeepAlive'd
# forever-loop only needs launchd to restart a dead process, which it does
# reliably. Manual invocations (no WATCHDOG_LOOP) run a single pass and exit.
# Backstops the node agents' own launchd KeepAlive (which won't respawn
# reliably in some launchd domains either — same failure family).
#
# DO NOT FLAP THE BACKEND. A single slow /healthz — a GC pause, a busy keep-warm
# tick, or a node still warming up right after a restart — must NEVER cause a
# kill. We only restart after the node fails EVERY probe across a long sustained
# window (~1 minute). A healthy node, or one that recovers mid-window, is left
# completely alone. (Earlier this used a 2s single-probe-then-SIGKILL check, which
# killed the backend on any transient slowness → restart loop. Never again.)
set -uo pipefail
DOMAIN="gui/$(id -u)"

# Tunables: require STRIKES consecutive failures, PROBE_TIMEOUT each, spaced by
# SLEEP_BETWEEN → ~STRIKES*(PROBE_TIMEOUT+SLEEP_BETWEEN)s of *sustained* downtime
# before any action. Generous on purpose — covers warm-up + transient stalls.
STRIKES=4
PROBE_TIMEOUT=8
SLEEP_BETWEEN=6

healthy() { curl -fsS -m"$PROBE_TIMEOUT" "http://127.0.0.1:$1/healthz" >/dev/null 2>&1; }

# Mesh-membership check (/v1/mesh): /healthz stays green during SILENT PARTIAL-
# MESH ISOLATION — the exact node-a/node-b incident class where a node serves
# fine locally but sees ZERO of its expected fleet (orphan launch, gossip
# signature rejection, stale identity). `isolated:true` means peers were
# expected and none are visible. Returns 0 = fine, 1 = isolated. A node with no
# expected peers (single-node dev), or an unreachable/old endpoint, counts as
# fine — this check must never ADD flappiness, only surface real isolation.
mesh_ok() {
  local body
  body=$(curl -fsS -m"$PROBE_TIMEOUT" "http://127.0.0.1:$1/v1/mesh" 2>/dev/null) || return 0
  case "$body" in
    *'"isolated":true'*) return 1 ;;
    *) return 0 ;;
  esac
}

# Impostor check: the hive-cloud process LISTENING on the admin port must be the
# one launched with this label's own --name. /healthz and /v1/mesh both pass when
# a DIFFERENT hive-cloud instance squats the port (live-hit 2026-07-21: a reboot's
# RunAtLoad race let the meshless dev `node-a` win fc-lax's 8786/8787; it answered
# /healthz green and saw LAN peers, so neither existing check fired while the real
# fc-lax crash-looped on bind for 22h and fleet build dispatch through LA failed).
# argv is deterministic — no sustained-window needed. Prints the squatter pid and
# returns 0 only when a WRONG-named hive-cloud holds the port; anything else
# (correct owner, non-hive process, no listener) returns 1 and is left alone.
imposter_pid() { # <expected-name> <admin-port>
  local pid args
  pid=$(lsof -nP -tiTCP:"$2" -sTCP:LISTEN 2>/dev/null | head -1)
  [ -n "$pid" ] || return 1
  args=$(ps -o command= -p "$pid" 2>/dev/null) || return 1
  case "$args" in
    *"--name $1 "*|*"--name $1") return 1 ;;
    *hive-cloud*) echo "$pid"; return 0 ;;
    *) return 1 ;;
  esac
}

ensure() { # <label> <admin-port>
  launchctl print "$DOMAIN/$1" >/dev/null 2>&1 || return 0   # not installed → skip

  # Evict a port squatter before any health interpretation: a wrong-named
  # hive-cloud on this port means every probe below is interrogating the WRONG
  # process. Bootout its own label if launchd owns it (stop + unload, so its
  # KeepAlive cannot re-race), plain kill for an orphan, then kickstart the
  # rightful label to claim the port immediately.
  local name pid ilabel
  name=${1#dev.shadw.}
  if pid=$(imposter_pid "$name" "$2"); then
    ilabel=$(launchctl list 2>/dev/null | awk -v p="$pid" '$1 == p {print $3}')
    echo "$(date '+%H:%M:%S') $1 IMPOSTOR on :$2 (pid $pid label ${ilabel:-none} is not --name $name) — evicting and restarting $1"
    if [ -n "$ilabel" ]; then
      launchctl bootout "$DOMAIN/$ilabel" 2>/dev/null || kill "$pid" 2>/dev/null || true
    else
      kill "$pid" 2>/dev/null || true
    fi
    sleep 2
    launchctl kickstart -k "$DOMAIN/$1" 2>/dev/null || true
    return 0
  fi

  # Fast path: healthy right now → done (the overwhelmingly common case).
  if healthy "$2"; then
    # Same sustained-window discipline for isolation: a single isolated read
    # (e.g. the first seconds after a restart, before gossip resyncs) must not
    # trigger anything. Only STRIKES consecutive isolated reads — with the node
    # otherwise healthy — earn a restart (a restart re-runs bootstrap/rejoin,
    # which is the standard recovery for the stale-identity/orphan-launch class).
    mesh_ok "$2" && return 0
    local j
    for ((j = 2; j <= STRIKES; j++)); do
      sleep "$SLEEP_BETWEEN"
      mesh_ok "$2" && return 0
    done
    echo "$(date '+%H:%M:%S') $1 ISOLATED on :$2 (/v1/mesh isolated=true across $STRIKES probes; /healthz green) — restarting to rejoin mesh"
    launchctl kickstart -k "$DOMAIN/$1" 2>/dev/null || true
    return 0
  fi

  # Not healthy on the first probe. Could be warming up or a transient stall —
  # re-probe several times; the moment it answers, we're done and never touch it.
  local i
  for ((i = 2; i <= STRIKES; i++)); do
    sleep "$SLEEP_BETWEEN"
    healthy "$2" && return 0
  done

  # Failed every probe across the whole window → genuinely down/wedged. `-k` is
  # required to clear a wedged process that won't exit on its own; the sustained
  # window above guarantees we only reach here for a truly dead node.
  echo "$(date '+%H:%M:%S') $1 down on :$2 (failed $STRIKES probes over ~$((STRIKES * (PROBE_TIMEOUT + SLEEP_BETWEEN)))s) — restarting"
  launchctl kickstart -k "$DOMAIN/$1" 2>/dev/null || true
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
