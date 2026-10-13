#!/bin/bash
# EXTERNAL liveness watchdog for a headless browser node, run by a systemd timer BESIDE the browser-node unit, never inside
# it: a wedged supervisor still reads as `active` to systemd, and only a probe outside its process notices. It restarts the
# unit after BAD_PROBE_LIMIT consecutive failed /healthz probes (like the supervisor's DEGRADED_LIMIT, it never reacts to a
# single bad sample).
set -u

ORIGIN="${HIVE_BROWSER_NODE_ORIGIN:-http://127.0.0.1:3009}"
BAD_PROBE_LIMIT="${HIVE_BROWSER_NODE_WATCHDOG_LIMIT:-6}"
STATE="${HIVE_BROWSER_NODE_WATCHDOG_STATE:-/run/hive-browser-node-watchdog.count}"
UNIT="${HIVE_BROWSER_NODE_UNIT:-hive-browser-node.service}"
PROBE_TIMEOUT_SECS=5

code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time "$PROBE_TIMEOUT_SECS" "${ORIGIN}/healthz" 2>/dev/null)" || code=000

bad_probes=0
if [[ -r "$STATE" ]]; then bad_probes="$(cat "$STATE" 2>/dev/null || echo 0)"; fi
[[ "$bad_probes" =~ ^[0-9]+$ ]] || bad_probes=0

reset_bad_probe_count() { echo 0 >"$STATE" 2>/dev/null || true; }

if [[ "$code" == "200" ]]; then
  if (( bad_probes != 0 )); then
    echo "hive-browser-node-watchdog: healthy again after ${bad_probes} bad probe(s)"
  fi
  reset_bad_probe_count
  exit 0
fi

bad_probes=$((bad_probes + 1))
echo "$bad_probes" >"$STATE" 2>/dev/null || true
echo "hive-browser-node-watchdog: not healthy (HTTP ${code}) ${bad_probes}/${BAD_PROBE_LIMIT}" >&2

if (( bad_probes >= BAD_PROBE_LIMIT )); then
  reset_bad_probe_count
  echo "hive-browser-node-watchdog: restarting ${UNIT} after ${bad_probes} consecutive bad probes" >&2
  systemctl restart "$UNIT"
fi
exit 0
