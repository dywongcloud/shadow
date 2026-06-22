#!/usr/bin/env bash
# Watchdog for the shadw node daemons: ensures each installed node answers on its
# admin /healthz; `kickstart`s any that are down. Run periodically by the
# dev.shadw.watchdog LaunchAgent (StartInterval). This backstops launchd KeepAlive
# (which, in some launchd domains, won't respawn reliably) so a crashed node is
# always brought back within one interval.
set -uo pipefail
DOMAIN="gui/$(id -u)"

ensure() { # <label> <admin-port>
  launchctl print "$DOMAIN/$1" >/dev/null 2>&1 || return 0   # not installed → skip
  curl -fsS -m2 "http://127.0.0.1:$2/healthz" >/dev/null 2>&1 && return 0
  echo "$(date '+%H:%M:%S') $1 down on :$2 — kickstarting"
  launchctl kickstart -k "$DOMAIN/$1" 2>/dev/null || true
}

ensure dev.shadw.node-a 8786
ensure dev.shadw.node-b 8788
