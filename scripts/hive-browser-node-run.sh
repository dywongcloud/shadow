#!/usr/bin/env bash
# Launch + supervise the headless Chromium that IS the fleet browser node (single source here; the
# hive_browser_node role copies it to /opt/hive-browser-node/bin/). systemd only sees a chrome
# exit; the broker's /healthz sees a dead NODE inside a live process, so this wrapper polls it and
# recycles the browser only after consecutive failures. Every knob arrives via the environment.
set -euo pipefail

CHROME_BIN="${HIVE_BROWSER_NODE_CHROME:-/usr/bin/google-chrome-stable}"
PROFILE_DIR="${HIVE_BROWSER_NODE_PROFILE:-/var/lib/hive/browser-node/profile}"
PORT="${HIVE_BROWSER_NODE_PORT:-3009}"
ORIGIN="http://127.0.0.1:${PORT}"
# Under the unit's MemoryMax, so V8 reclaims before the cgroup OOM-kills the process.
JS_HEAP_MB="${HIVE_BROWSER_NODE_JS_HEAP_MB:-512}"
BROKER_WAIT_SECS="${HIVE_BROWSER_NODE_BROKER_WAIT_SECS:-120}"
HEALTH_POLL_SECS="${HIVE_BROWSER_NODE_HEALTH_POLL_SECS:-30}"
UNREACHABLE_LIMIT="${HIVE_BROWSER_NODE_HEALTH_FAIL_LIMIT:-4}"
# Far longer than a lease renewal (<=300s) plus an admission retry: degraded clears itself, and
# recycling a degraded node is a restart loop.
DEGRADED_LIMIT="${HIVE_BROWSER_NODE_DEGRADED_LIMIT:-40}"
DEGRADED_LOG_EVERY="${HIVE_BROWSER_NODE_DEGRADED_LOG_EVERY:-10}"
EX_SOFTWARE=70
EX_TEMPFAIL=75

if [[ ! -x "$CHROME_BIN" ]]; then
  echo "hive-browser-node: chrome binary not found/executable: $CHROME_BIN" >&2
  exit 1
fi

mkdir -p "$PROFILE_DIR"

# No `-f`: this only asks whether the broker ANSWERS. /healthz is 503 until a browser is admitted,
# so `curl -f` would never terminate on any restart that is not a cold boot of both.
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

# A stale singleton lock from a SIGKILLed predecessor makes chrome exit at once.
rm -f "${PROFILE_DIR}/SingletonLock" "${PROFILE_DIR}/SingletonCookie" "${PROFILE_DIR}/SingletonSocket" 2>/dev/null || true

# The liveness flags are mandatory: a headless page is occluded, and without them Chromium
# throttles its timers to ~1/minute, breaking the presence republish and the lease renewal.
# --no-sandbox is deliberately absent: the unit runs unprivileged with user namespaces available.
# shellcheck disable=SC2054  # commas inside single --disable-features/--js-flags values
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
    # `|| rc=$?`, never `|| true`: the latter reports 0 for every death, however violent.
    rc=0
    wait "$CHROME_PID" || rc=$?
    echo "hive-browser-node: chrome exited (rc=${rc}); letting systemd restart the unit" >&2
    exit "$(( rc == 0 ? EX_SOFTWARE : rc ))"
  fi
  # Any HTTP status proves the broker is alive (degraded is waited out); only no answer at all
  # (000) is a wedge recycling can repair.
  code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 "${ORIGIN}/healthz" 2>/dev/null)" || code=000
  if [[ "$code" != "000" ]]; then
    if (( fails > 0 )); then
      echo "hive-browser-node: broker reachable again after ${fails} unreachable probe(s)"
    fi
    fails=0
    if [[ "$code" == "200" ]]; then
      degraded=0
      continue
    fi
    degraded=$((degraded + 1))
    if (( degraded % DEGRADED_LOG_EVERY == 1 )); then
      detail="$(curl -sS --max-time 5 "${ORIGIN}/healthz" 2>/dev/null || echo '{}')"
      echo "hive-browser-node: degraded (HTTP ${code}) for $((degraded * HEALTH_POLL_SECS))s, waiting for self-heal: ${detail}" >&2
    fi
    if (( degraded >= DEGRADED_LIMIT )); then
      echo "hive-browser-node: still degraded after $((degraded * HEALTH_POLL_SECS))s — recycling chrome" >&2
      cleanup
      exit "$EX_TEMPFAIL"
    fi
    continue
  fi
  degraded=0
  fails=$((fails + 1))
  echo "hive-browser-node: broker unreachable, probe ${fails}/${UNREACHABLE_LIMIT}" >&2
  if (( fails >= UNREACHABLE_LIMIT )); then
    echo "hive-browser-node: broker unreachable for $((fails * HEALTH_POLL_SECS))s — recycling chrome" >&2
    cleanup
    exit "$EX_TEMPFAIL"
  fi
done
