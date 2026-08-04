#!/usr/bin/env bash
# Live, no-mocks witness for bn-browser-fleet-crr-exchange: a REAL two-process
# setup — a local hive-cloud (HIVE_FORCE_MOCK=1, enforced JWT, embedded relay)
# with real zip deploys carrying `browser_db` blocks, plus a REAL Chrome tab
# (headless=new; same engine/OPFS/wasm as headed) running the sqlite worker +
# BrowserNode wasm through the wire op — driven by witness-crr.mjs over CDP.
#
#   crates/hive-browser/www/sqlite/witness-crr.sh
#
# Prints WITNESS_OK only when every scenario assertion held (see the .mjs for
# the scenario list: convergence both ways, reload persistence, gap refusal,
# caps, revocation).
set -euo pipefail
cd "$(dirname "$0")"
REPO_ROOT=../../../..
cd "$REPO_ROOT"
REPO_ROOT=$(pwd)

WORK="$(mktemp -d /tmp/crr-witness-XXXXXX)"
ADMIN_PORT=28786
PUBLIC_PORT=28080
DNS_PORT=25353
TLS_PORT=28443
RELAY_PORT=23341
STATIC_PORT=28321
JWT_SECRET="witness-secret-$(date +%s)"
PIDS=()

cleanup() {
  for pid in "${PIDS[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT INT TERM

echo "== build hive-cloud + replica_tool"
cargo build -q -p hive-cloud
cargo build -q -p hive-crsql --example replica_tool

echo "== start local hive-cloud (mock backend, enforced JWT, embedded relay)"
HIVE_FORCE_MOCK=1 \
HIVE_JWT_SECRET="$JWT_SECRET" \
HIVE_DATA="$WORK/data" \
HIVE_INGRESS=ngrok \
HIVE_DNS_ADDR="127.0.0.1:${DNS_PORT}" \
HIVE_TLS_ADDR="127.0.0.1:${TLS_PORT}" \
HIVE_RELAY_URLS="http://127.0.0.1:${RELAY_PORT}" \
HIVE_OWN_RELAY_PORT="$RELAY_PORT" \
HIVE_BROWSER_SESSION_MAX_AGE_SECS=3600 \
RUST_LOG="${RUST_LOG:-info,iroh=warn,guardian_db=warn}" \
  ./target/debug/hive-cloud \
  --name witness-node --listen "127.0.0.1:${PUBLIC_PORT}" --admin "127.0.0.1:${ADMIN_PORT}" \
  >"$WORK/hive-cloud.log" 2>&1 &
PIDS+=($!)

for i in $(seq 1 60); do
  curl -fsS -m1 "http://127.0.0.1:${ADMIN_PORT}/healthz" >/dev/null 2>&1 && break
  sleep 0.5
  if [ "$i" = 60 ]; then echo "FAIL: hive-cloud never came up"; tail -30 "$WORK/hive-cloud.log"; exit 1; fi
done
curl -fsS -m2 "http://127.0.0.1:${RELAY_PORT}/healthz" >/dev/null 2>&1 || \
  echo "note: relay /healthz not answering (relay may still serve /relay — continuing)"

echo "== mint an enforced JWT (HS256, HIVE_JWT_SECRET)"
TOKEN=$(JWT_SECRET="$JWT_SECRET" node -e '
const crypto = require("crypto");
const b64 = (o) => Buffer.from(JSON.stringify(o)).toString("base64url");
const now = Math.floor(Date.now() / 1000);
const payload = { sub: "witness-user", tenant: "witnessteam", role: "owner",
  iat: now - 1, exp: now + 3600, platform_admin: true };
const unsigned = `${b64({ alg: "HS256", typ: "JWT" })}.${b64(payload)}`;
const sig = crypto.createHmac("sha256", process.env.JWT_SECRET).update(unsigned).digest("base64url");
console.log(`${unsigned}.${sig}`);
')

echo "== author the two witness projects (fluid.json + browser handler)"
author_project() {
  local dir="$1" project="$2" max_bytes="$3" max_value_bytes="$4"
  mkdir -p "$dir"
  cat > "$dir/fluid.json" <<EOF
{
  "project": "$project",
  "functions": [
    {
      "name": "api",
      "runtime": "bun",
      "start_cmd": ["bun", "run", "server.js"],
      "browser": { "entry": "handler.js" }
    }
  ],
  "browser_db": {
    "max_bytes": $max_bytes,
    "max_value_bytes": $max_value_bytes,
    "public_read": false,
    "schema": [
      { "name": "items",
        "ddl": "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY NOT NULL, label TEXT);" }
    ]
  }
}
EOF
  cat > "$dir/handler.js" <<'EOF'
module.exports = async function handler(request) {
  return { status: 200, headers: {}, body: 'ok' };
};
EOF
  cat > "$dir/server.js" <<'EOF'
module.exports = {};
EOF
  (cd "$dir" && zip -q -r "../$(basename "$dir").zip" .)
}
author_project "$WORK/main" "crrmain" 67108864 1048576
author_project "$WORK/caps" "crrcaps" 262144 65536

deploy() {
  local zip="$1" project="$2"
  local meta resp
  # A distinct filename per project: repo_url is `upload://<filename>` and the
  # build queue dedups on (repo_url, branch, commit) — two "archive.zip"
  # uploads can coalesce into one build.
  meta=$(printf '{"project":"%s","filename":"%s.zip"}' "$project" "$project" | base64)
  resp=$(curl -fsS -X POST "http://127.0.0.1:${ADMIN_PORT}/v1/deploy/zip" \
    -H "Authorization: Bearer $TOKEN" \
    -H "x-hive-deploy-meta: $meta" \
    -H "content-type: application/zip" \
    --data-binary "@$zip")
  echo "   deploy $project -> $resp"
}

deployment_id() {
  local project="$1"
  curl -fsS "http://127.0.0.1:${ADMIN_PORT}/deployments" \
    -H "Authorization: Bearer $TOKEN" | node -e '
let body = "";
process.stdin.on("data", (c) => (body += c)).on("end", () => {
  const parsed = JSON.parse(body);
  const list = parsed.deployments ?? parsed;
  const project = process.argv[1];
  const ready = list.find((d) => d.project === project && (d.state === "ready" || d.state === "Ready"));
  if (ready) { console.log(ready.id); process.exit(0); }
  const any = list.find((d) => d.project === project);
  if (any) console.error(`${project}: state=${any.state}`);
  process.exit(1);
});' "$project"
}

echo "== deploy both projects as real zips (serialized: a concurrent two-zip"
echo "   race in the build path intermittently strands the second build)"
deploy "$WORK/main.zip" "crrmain"

DEPLOY_MAIN=""
DEPLOY_CAPS=""
for i in $(seq 1 120); do
  [ -z "$DEPLOY_MAIN" ] && DEPLOY_MAIN=$(deployment_id "crrmain" 2>/dev/null || true)
  [ -n "$DEPLOY_MAIN" ] && break
  sleep 0.5
  if [ "$i" = 120 ]; then
    echo "FAIL: crrmain never became Ready"
    curl -fsS "http://127.0.0.1:${ADMIN_PORT}/deployments" -H "Authorization: Bearer $TOKEN" || true
    tail -50 "$WORK/hive-cloud.log"
    exit 1
  fi
done
echo "   crrmain=$DEPLOY_MAIN"

deploy "$WORK/caps.zip" "crrcaps"
for i in $(seq 1 120); do
  [ -z "$DEPLOY_CAPS" ] && DEPLOY_CAPS=$(deployment_id "crrcaps" 2>/dev/null || true)
  [ -n "$DEPLOY_CAPS" ] && break
  sleep 0.5
  if [ "$i" = 120 ]; then
    echo "FAIL: crrcaps never became Ready"
    echo "== /deployments at failure:"
    curl -fsS "http://127.0.0.1:${ADMIN_PORT}/deployments" -H "Authorization: Bearer $TOKEN" || true
    echo
    tail -50 "$WORK/hive-cloud.log"
    exit 1
  fi
done
echo "   crrcaps=$DEPLOY_CAPS"

echo "== static-serve crates/hive-browser/www (+ same-origin /api proxy)"
node "$REPO_ROOT/crates/hive-browser/www/sqlite/witness-server.mjs" \
  "$STATIC_PORT" "$REPO_ROOT/crates/hive-browser/www" "http://127.0.0.1:${ADMIN_PORT}" \
  >"$WORK/static.log" 2>&1 &
PIDS+=($!)

DB_MAIN="$WORK/data/browser-dbs/hive-browserdb-crrmain.db"
DB_CAPS="$WORK/data/browser-dbs/hive-browserdb-crrcaps.db"

echo "== wait for the browser_db reconcile to create both replica files"
for i in $(seq 1 180); do
  [ -f "$DB_MAIN" ] && [ -f "$DB_CAPS" ] && break
  sleep 0.5
  if [ "$i" = 180 ]; then
    echo "FAIL: replica files never appeared ($DB_MAIN / $DB_CAPS)"
    tail -30 "$WORK/hive-cloud.log"
    exit 1
  fi
done
ls -la "$WORK/data/browser-dbs/"

echo "== drive the real-Chrome witness"
node "$REPO_ROOT/crates/hive-browser/www/sqlite/witness-crr.mjs" \
  --api "http://127.0.0.1:${STATIC_PORT}/api" \
  --token "$TOKEN" \
  --relay "http://127.0.0.1:${RELAY_PORT}" \
  --deployment-main "$DEPLOY_MAIN" \
  --deployment-caps "$DEPLOY_CAPS" \
  --db-main "$DB_MAIN" \
  --db-caps "$DB_CAPS" \
  --replica-tool "$REPO_ROOT/target/debug/examples/replica_tool" \
  --page "http://127.0.0.1:${STATIC_PORT}/sqlite/witness-crr.html"
RC=$?

if [ "$RC" = 0 ]; then
  echo "== stale-node contrast: restart hive-cloud with HIVE_BROWSER_DB_LISTEN=0"
  kill "${PIDS[0]}" 2>/dev/null || true
  wait "${PIDS[0]}" 2>/dev/null || true
  HIVE_FORCE_MOCK=1 \
  HIVE_JWT_SECRET="$JWT_SECRET" \
  HIVE_DATA="$WORK/data" \
  HIVE_INGRESS=ngrok \
  HIVE_DNS_ADDR="127.0.0.1:${DNS_PORT}" \
  HIVE_TLS_ADDR="127.0.0.1:${TLS_PORT}" \
  HIVE_RELAY_URLS="http://127.0.0.1:${RELAY_PORT}" \
  HIVE_OWN_RELAY_PORT="$RELAY_PORT" \
  HIVE_BROWSER_SESSION_MAX_AGE_SECS=3600 \
  HIVE_BROWSER_DB_LISTEN=0 \
  RUST_LOG="${RUST_LOG:-info,iroh=warn,guardian_db=warn}" \
    ./target/debug/hive-cloud \
    --name witness-node --listen "127.0.0.1:${PUBLIC_PORT}" --admin "127.0.0.1:${ADMIN_PORT}" \
    >"$WORK/hive-cloud-stale.log" 2>&1 &
  PIDS[0]=$!
  for i in $(seq 1 60); do
    curl -fsS -m1 "http://127.0.0.1:${ADMIN_PORT}/healthz" >/dev/null 2>&1 && break
    sleep 0.5
    if [ "$i" = 60 ]; then echo "FAIL: stale hive-cloud never came up"; tail -30 "$WORK/hive-cloud-stale.log"; exit 1; fi
  done
  node "$REPO_ROOT/crates/hive-browser/www/sqlite/witness-crr.mjs" \
    --scenario stale \
    --api "http://127.0.0.1:${STATIC_PORT}/api" \
    --token "$TOKEN" \
    --relay "http://127.0.0.1:${RELAY_PORT}" \
    --deployment-main "$DEPLOY_MAIN" \
    --deployment-caps "$DEPLOY_CAPS" \
    --db-main "$DB_MAIN" \
    --db-caps "$DB_CAPS" \
    --replica-tool "$REPO_ROOT/target/debug/examples/replica_tool" \
    --page "http://127.0.0.1:${STATIC_PORT}/sqlite/witness-crr.html"
  RC=$?
fi

if [ "$RC" != 0 ]; then
  echo "== hive-cloud log tail (witness failed)"
  tail -40 "$WORK/hive-cloud.log" || true
fi
echo "work dir: $WORK"
exit "$RC"
