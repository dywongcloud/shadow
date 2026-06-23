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

ensure() { # <label> <admin-port>
  launchctl print "$DOMAIN/$1" >/dev/null 2>&1 || return 0   # not installed → skip

  # Fast path: healthy right now → done (the overwhelmingly common case).
  healthy "$2" && return 0

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
