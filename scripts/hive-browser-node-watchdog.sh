#!/bin/bash
# EXTERNAL liveness watchdog for a headless browser node.
#
# Why this exists, and why it is deliberately OUTSIDE the browser-node unit.
#
# The supervisor in `hive-browser-node-run.sh` already polls the broker's
# /healthz and recycles Chrome when the node stays degraded past its limit.
# That covers a sick NODE. It cannot cover a sick SUPERVISOR, and on
# 2026-08-08 fc-bangkok demonstrated the difference:
#
#   * chrome sat in state `DNl` -- uninterruptible sleep, unkillable until its
#     I/O completed;
#   * the supervisor bash blocked in `anon_pipe_read` (a command substitution
#     whose pipe never reached EOF, because a wedged child still held it);
#   * its health loop therefore emitted NOTHING for 5.5 hours -- no degraded
#     lines, no recycle, no exit;
#   * and because the parent bash was still alive, systemd reported the unit
#     `active` with `NRestarts=0`.
#
# The node had published no presence for 17 hours and no supervisor message for
# 2 hours while every dashboard showed it simply absent. A watchdog running
# inside that process is structurally incapable of noticing; one running beside
# it notices immediately, because it shares none of the wedged descriptors.
#
# Restart policy: only after LIMIT CONSECUTIVE bad probes. At the shipped
# cadence that window is far longer than a lease renewal or an admission
# retry, so ordinary self-healing is never interrupted -- the same discipline
# the supervisor's own DEGRADED_LIMIT follows, and the reason neither one
# reacts to a single bad sample.
set -u

ORIGIN="${HIVE_BROWSER_NODE_ORIGIN:-http://127.0.0.1:3009}"
LIMIT="${HIVE_BROWSER_NODE_WATCHDOG_LIMIT:-6}"
STATE="${HIVE_BROWSER_NODE_WATCHDOG_STATE:-/run/hive-browser-node-watchdog.count}"
UNIT="${HIVE_BROWSER_NODE_UNIT:-hive-browser-node.service}"

# `|| echo 000` keeps a refused/timed-out probe a FAILURE rather than a script
# error; --max-time bounds it so the watchdog can never inherit the very stall
# it is here to break.
code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 "${ORIGIN}/healthz" 2>/dev/null || echo 000)"

n=0
if [[ -r "$STATE" ]]; then n="$(cat "$STATE" 2>/dev/null || echo 0)"; fi
[[ "$n" =~ ^[0-9]+$ ]] || n=0

if [[ "$code" == "200" ]]; then
  if (( n != 0 )); then
    echo "hive-browser-node-watchdog: healthy again after ${n} bad probe(s)"
  fi
  echo 0 >"$STATE" 2>/dev/null || true
  exit 0
fi

n=$((n + 1))
echo "$n" >"$STATE" 2>/dev/null || true
echo "hive-browser-node-watchdog: not healthy (HTTP ${code}) ${n}/${LIMIT}" >&2

if (( n >= LIMIT )); then
  # Reset FIRST: if the restart itself fails, the next pass starts a fresh
  # count instead of restarting on every subsequent probe.
  echo 0 >"$STATE" 2>/dev/null || true
  echo "hive-browser-node-watchdog: restarting ${UNIT} after ${n} consecutive bad probes" >&2
  systemctl restart "$UNIT"
fi
exit 0
