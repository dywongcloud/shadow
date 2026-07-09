#!/usr/bin/env bash
# Watchdog for the shadw node daemons: ensures each installed node answers on its
# admin /healthz; restarts ONLY a node that is genuinely down/wedged. Run
# periodically by the dev.shadw.watchdog LaunchAgent (StartInterval). Backstops
# launchd KeepAlive (which won't respawn reliably in some launchd domains).
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

ensure() { # <label> <admin-port>
  launchctl print "$DOMAIN/$1" >/dev/null 2>&1 || return 0   # not installed → skip

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

ensure dev.shadw.node-a 8786
ensure dev.shadw.node-b 8788
