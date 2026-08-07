#!/usr/bin/env bash
# Launch + supervise the headless Chromium that IS the fleet browser node.
# Single canonical source in scripts/; the `hive_browser_node` ansible role
# copies it to /opt/hive-browser-node/bin/ -- never hand-edit a second copy.
#
# Why a wrapper rather than ExecStart=chrome directly: the failure mode that
# matters is not "chrome exited" (systemd Restart=always already covers that),
# it is "chrome is still running but the node is dead" -- a wasm panic that
# takes the worker's global scope with it, an admission wedged in a terminal
# denial, a page that stopped publishing presence. systemd cannot see any of
# those. The broker can (the page heartbeats it), so this wrapper polls the
# broker's /healthz and recycles the browser when the NODE, not the process,
# has died. Consecutive-failure counting is deliberate: a relay migration or a
# leader election briefly reports degraded, and a single bad sample must never
# recycle a healthy browser.
#
# Every knob arrives via the environment (systemd EnvironmentFile); there are
# no positional arguments.
set -euo pipefail

CHROME_BIN="${HIVE_BROWSER_NODE_CHROME:-/usr/bin/google-chrome-stable}"
PROFILE_DIR="${HIVE_BROWSER_NODE_PROFILE:-/var/lib/hive/browser-node/profile}"
PORT="${HIVE_BROWSER_NODE_PORT:-3009}"
ORIGIN="http://127.0.0.1:${PORT}"
# JS heap ceiling handed to V8. Kept well under the unit's MemoryMax so V8
# reclaims under its OWN pressure before the cgroup OOM-kills the process --
# a GC pause is recoverable, a cgroup kill loses the node's admission.
JS_HEAP_MB="${HIVE_BROWSER_NODE_JS_HEAP_MB:-512}"
BROKER_WAIT_SECS="${HIVE_BROWSER_NODE_BROKER_WAIT_SECS:-120}"
HEALTH_POLL_SECS="${HIVE_BROWSER_NODE_HEALTH_POLL_SECS:-30}"
# Consecutive UNREACHABLE probes (no HTTP response at all) before recycling
# chrome. Unreachable means the broker is wedged, which a restart genuinely
# repairs, so this stays fast: 4 x 30s = 2min.
HEALTH_FAIL_LIMIT="${HIVE_BROWSER_NODE_HEALTH_FAIL_LIMIT:-4}"
# Consecutive DEGRADED probes (broker answering, but not 200) before recycling.
# Deliberately far longer than the 4-probe unreachable limit AND far longer than
# a lease renewal (<=300s) plus an admission retry, because degraded is the
# state the node clears by itself — 40 x 30s = 20min. Anything shorter turns
# ordinary self-healing into a restart loop, which is exactly what the old
# shared 4-probe counter did.
DEGRADED_LIMIT="${HIVE_BROWSER_NODE_DEGRADED_LIMIT:-40}"
# Log the degraded detail on the 1st, then every Nth probe, so a long wait
# leaves evidence without flooding the journal every 30s.
DEGRADED_LOG_EVERY="${HIVE_BROWSER_NODE_DEGRADED_LOG_EVERY:-10}"

if [[ ! -x "$CHROME_BIN" ]]; then
  echo "hive-browser-node: chrome binary not found/executable: $CHROME_BIN" >&2
  exit 1
fi

mkdir -p "$PROFILE_DIR"

# The broker owns the session mint and the static bundle; starting chrome
# before it answers just burns a restart cycle on a page that 502s.
#
# NO `-f` HERE, and it is load-bearing. This wait only asks "is the broker
# ANSWERING"; whether the NODE is healthy is by definition unknowable before
# the browser it is about to start has ever run. /healthz returns 503 for an
# unhealthy node, and `curl -f` turns that into exit 22 -- so with `-f` the
# loop could never terminate in the one case that matters most: the broker
# already up and PAST its boot grace with no live browser behind it. That is
# every restart of this unit that is not a cold boot of both -- the daily
# RuntimeMaxSec recycle, a supervisor-initiated recycle, a crash loop -- and
# it wedged deterministically: 120s of waiting, exit 1, systemd restarts,
# repeat, browser node permanently dark while the broker looks fine.
# Reproduced live before the fix (503 -> `curl -fsS` exit 22 -> chrome never
# launched). The health-poll loop below keeps `-f` precisely because there a
# 503 IS the signal.
echo "hive-browser-node: waiting for broker at ${ORIGIN}/healthz (<= ${BROKER_WAIT_SECS}s)"
waited=0
until curl -sS --max-time 3 "${ORIGIN}/healthz" >/dev/null 2>&1; do
  sleep 2
  waited=$((waited + 2))
  if (( waited >= BROKER_WAIT_SECS )); then
    echo "hive-browser-node: broker never became reachable" >&2
    exit 1
  fi
done

CHROME_PID=""
cleanup() {
  if [[ -n "$CHROME_PID" ]] && kill -0 "$CHROME_PID" 2>/dev/null; then
    kill -TERM "$CHROME_PID" 2>/dev/null || true
    for _ in $(seq 1 10); do
      kill -0 "$CHROME_PID" 2>/dev/null || break
      sleep 1
    done
    kill -KILL "$CHROME_PID" 2>/dev/null || true
  fi
}
trap 'cleanup; exit 143' TERM INT

# A stale singleton lock from a SIGKILLed predecessor makes chrome refuse the
# profile ("The profile appears to be in use by another Chrome process") and
# exit immediately -- an infinite restart loop with a one-line cause.
rm -f "${PROFILE_DIR}/SingletonLock" "${PROFILE_DIR}/SingletonCookie" "${PROFILE_DIR}/SingletonSocket" 2>/dev/null || true

# --- flags -------------------------------------------------------------------
# Resource-shaping (the browser's own half of the caps; the cgroup is the other
# half and is the hard bound):
#   --renderer-process-limit=1   one page, so one renderer; never a fan-out
#   --js-flags=--max-old-space-size  V8 heap ceiling under the cgroup's MemoryMax
#   --disk-cache-size            bounded on-disk cache (the profile dir is the
#                                unit's only ReadWritePath)
# Liveness (NOT optional -- a headless page is "occluded" by definition, and
# with these omitted Chromium throttles background timers to ~1/minute, which
# breaks BOTH the 45s presence republish and the worker's 60s lease renewal;
# the node then ages off the constellation while the process looks perfectly
# healthy):
#   --disable-background-timer-throttling
#   --disable-backgrounding-occluded-windows
#   --disable-renderer-backgrounding
#   --disable-features=CalculateNativeWinOcclusion
# Server hygiene: no keyring/dbus probe, no crash upload, no component updates,
# no first-run UI.
#
# NOT passed, deliberately: --no-sandbox. The role verifies unprivileged user
# namespaces are available and the unit runs as a non-root user, so Chromium's
# namespace sandbox stays ON. Disabling it would put tenant browser functions
# (untrusted JS, by design) one V8 bug away from the host that also runs
# hive-node.
# shellcheck disable=SC2054  # the commas live INSIDE single --disable-features
# and --js-flags values; they are not element separators.
FLAGS=(
  --headless=new
  --user-data-dir="$PROFILE_DIR"
  --disk-cache-dir="${PROFILE_DIR}/cache"
  --disk-cache-size=134217728
  --renderer-process-limit=1
  --js-flags="--max-old-space-size=${JS_HEAP_MB}"
  --disable-background-timer-throttling
  --disable-backgrounding-occluded-windows
  --disable-renderer-backgrounding
  --disable-features=CalculateNativeWinOcclusion,Translate,MediaRouter,OptimizationHints,InterestFeedContentSuggestions
  --disable-gpu
  --disable-extensions
  --disable-component-update
  --disable-domain-reliability
  --disable-background-networking
  --disable-breakpad
  --disable-crash-reporter
  --metrics-recording-only
  --no-pings
  --no-first-run
  --no-default-browser-check
  --mute-audio
  --password-store=basic
  --use-mock-keychain
  --window-size=1280,800
  --enable-logging=stderr
  --log-level=2
)

echo "hive-browser-node: starting ${CHROME_BIN} -> ${ORIGIN}/ (js-heap=${JS_HEAP_MB}M)"
"$CHROME_BIN" "${FLAGS[@]}" "${ORIGIN}/" &
CHROME_PID=$!

fails=0
degraded=0
while true; do
  sleep "$HEALTH_POLL_SECS"
  if ! kill -0 "$CHROME_PID" 2>/dev/null; then
    # `wait ... || true` CLOBBERS $?: the `|| true` is the last command, so the
    # following `rc=$?` reads 0 for every death, however violent. Measured in
    # isolation on this same bash: a child whose true status is 143 reports 0
    # through that shape and 143 through this one. It matters precisely for the
    # deaths that are not clean -- chrome traps SIGTERM and genuinely does exit
    # 0, but a renderer OOM-kill (137), a segfault (139) and any non-zero exit
    # were all flattened to "rc=0" as well, and the supervisor then exited 0, so
    # `systemctl status` reported success for a browser node that had crashed.
    rc=0
    wait "$CHROME_PID" || rc=$?
    echo "hive-browser-node: chrome exited (rc=${rc}); letting systemd restart the unit" >&2
    # Chrome exiting is NEVER expected here, so even a clean rc=0 is a failed
    # run of this unit's job. Map it to EX_SOFTWARE so the journal and
    # `systemctl status` show a failure rather than a success; Restart=always
    # brings the node back either way.
    exit "$(( rc == 0 ? 70 : rc ))"
  fi
  # Two DIFFERENT failures, deliberately handled differently.
  #
  # This loop used to probe with `curl -fsS`, where -f turns any HTTP >= 400
  # into a failure. The broker answers 503 with a JSON body whenever the node is
  # merely DEGRADED (admission renewing, an artifact briefly unavailable, a
  # stale heartbeat) — so a live, answering broker was counted as a dead node,
  # and four polls later the supervisor killed Chrome. That is the churn: hk
  # recycled 21 times, sp 5, fr 3, while the underlying condition was one the
  # node clears BY ITSELF on the next lease renewal. Recycling does not fix a
  # degraded admission; it throws away a warm browser — pinned artifacts, the
  # OPFS replica, mesh sessions — and the fresh boot can 503 again on its own
  # admission, which is how a transient blip became a restart loop.
  #
  # So: an HTTP RESPONSE of any status proves the broker process is alive and
  # answering, which is the only thing recycling Chrome could repair. Degraded
  # is reported and waited out. Only a broker that does not answer at all
  # (connection refused / timeout — `curl -sS -o /dev/null -w %{http_code}`
  # yields 000) is a wedge worth recycling for, on the original fast counter.
  code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 "${ORIGIN}/healthz" 2>/dev/null || echo 000)"
  if [[ "$code" != "000" ]]; then
    if (( fails > 0 )); then
      echo "hive-browser-node: broker reachable again after ${fails} unreachable probe(s)"
    fi
    fails=0
    if [[ "$code" == "200" ]]; then
      degraded=0
      continue
    fi
    # Reachable but not healthy. Wait it out — but do not wait FOREVER: a node
    # wedged degraded indefinitely is genuinely stuck, and that window is
    # deliberately far longer than a lease renewal so normal self-healing is
    # never interrupted.
    degraded=$((degraded + 1))
    if (( degraded % DEGRADED_LOG_EVERY == 1 )); then
      detail="$(curl -sS --max-time 5 "${ORIGIN}/healthz" 2>/dev/null || echo '{}')"
      echo "hive-browser-node: degraded (HTTP ${code}) for $((degraded * HEALTH_POLL_SECS))s, waiting for self-heal: ${detail}" >&2
    fi
    if (( degraded >= DEGRADED_LIMIT )); then
      echo "hive-browser-node: still degraded after $((degraded * HEALTH_POLL_SECS))s — recycling chrome" >&2
      cleanup
      exit 75   # EX_TEMPFAIL — systemd Restart=always brings it back
    fi
    continue
  fi
  degraded=0
  fails=$((fails + 1))
  echo "hive-browser-node: broker unreachable, probe ${fails}/${HEALTH_FAIL_LIMIT}" >&2
  if (( fails >= HEALTH_FAIL_LIMIT )); then
    echo "hive-browser-node: broker unreachable for $((fails * HEALTH_POLL_SECS))s — recycling chrome" >&2
    cleanup
    exit 75   # EX_TEMPFAIL — systemd Restart=always brings it back
  fi
done
