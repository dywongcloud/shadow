#!/usr/bin/env bash
# Live witness: real hive-cloud node + real Lakebed capsule example.
# A live-execution witness (not a test file): real binary, real storage, real capsule.
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="$REPO/examples/lakebed-todo"
ADMIN=8919; PUBLIC=8918; DNS=5392; TLS=8492; APP=8920
API="http://127.0.0.1:$ADMIN"
DATA="$(mktemp -d /private/tmp/lakebed-witness.XXXXXX)"
NODE="$REPO/target/debug/hive-cloud"
NODE_PID=""; APP_PID=""; PASS=0; FAIL=0
cleanup() {
  [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null
  rm -rf "$DATA" 2>/dev/null || true
}
trap cleanup EXIT
ok()   { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

echo "==> real hive-cloud on isolated ports (data=$DATA)"
HIVE_DATA="$DATA" HIVE_DNS_ADDR="127.0.0.1:$DNS" HIVE_TLS_ADDR="127.0.0.1:$TLS" \
  HIVE_GUARDIAN_RESTORE=0 RUST_LOG=warn \
  "$NODE" --name lakebed-witness --listen "127.0.0.1:$PUBLIC" --admin "127.0.0.1:$ADMIN" \
  > "$DATA/node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 1 60); do
  curl -s -m1 "$API/healthz" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -s -m2 "$API/healthz" | grep -q . || { echo "node did not come up"; tail -20 "$DATA/node.log"; exit 1; }
ok "real node /healthz"

echo "==> framework catalog exposes lakebed"
FW="$(curl -s -m5 "$API/v1/build/frameworks")"
python3 -I -c 'import sys,json; d=json.load(sys.stdin); assert any(p.get("slug")=="lakebed" for p in d), "no lakebed preset"' <<<"$FW" \
  && ok "GET /v1/build/frameworks contains lakebed" || bad "lakebed preset missing"

echo "==> provision blob database + credential (values never printed)"
KEY="$(curl -s -m5 -X POST "$API/v1/apikeys" -H 'content-type: application/json' \
  -d '{"name":"lakebed-witness","role":"owner"}' \
  | python3 -I -c 'import sys,json;print(json.load(sys.stdin).get("token",""))')"
[ -n "$KEY" ] || { echo "no api key"; exit 1; }
DB="$(curl -s -m5 -X POST "$API/v1/databases" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' \
  -d '{"name":"lakebed-witness","project":"","kind":"blob"}')"
DBID="$(python3 -I -c 'import sys,json;print(json.load(sys.stdin).get("id",""))' <<<"$DB")"
[ -n "$DBID" ] || { echo "no database id"; exit 1; }
# The create response carries no connection; the bucket is only resolved by the
# readiness callback and is readable from the credentials route afterwards.
BUCKET=""
for _ in $(seq 1 12); do
  CREDS="$(curl -s -m5 "$API/v1/databases/$DBID/credentials" -H "authorization: Bearer $KEY")"
  BUCKET="$(python3 -I -c '
import sys,json
try:
 d=json.load(sys.stdin); c=d.get("connection") or d
 print(c.get("bucket",""))
except Exception: print("")
' <<<"$CREDS")"
  [ -n "$BUCKET" ] && break
  sleep 0.4
done
[ -n "$DBID" ] && [ -n "$BUCKET" ] && ok "blob database provisioned (id/bucket captured)" \
  || { echo "provision failed: no bucket"; exit 1; }

echo "==> start the capsule example on its own port"
SHADOW_API_BASE="$API" BLOB_BUCKET="$BUCKET" HIVE_API_TOKEN="$KEY" \
  BLOB_DATABASE_ID="$DBID" PORT="$APP" \
  node "$EX/server/serve.mjs" > "$DATA/app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 30); do
  curl -s -m1 "http://127.0.0.1:$APP/api/health" >/dev/null 2>&1 && break
  sleep 0.3
done
H="$(curl -s -m3 "http://127.0.0.1:$APP/api/health")"
python3 -I -c 'import sys,json;d=json.load(sys.stdin);assert d.get("ok") is True and d.get("framework")=="lakebed"' <<<"$H" \
  && ok "capsule /api/health reports lakebed" || { bad "capsule health"; tail -20 "$DATA/app.log"; }

APP="http://127.0.0.1:$APP"
post() { curl -s -m8 -X POST "$APP/api/$1/$2" -H 'content-type: application/json' -d "$3"; }

echo "==> database binding: insert -> list -> count -> toggle -> delete"
ADD="$(post mutation add '{"text":"ship lakebed","ownerId":"witness"}')"
ID="$(python3 -I -c 'import sys,json;d=json.load(sys.stdin);assert d.get("ok");print(d["value"]["id"])' <<<"$ADD")"
[ ${#ID} -eq 64 ] && ok "add mutation wrote a 64-hex row id" || bad "add mutation ($ADD)"

LIST="$(post query list '{"ownerId":"witness"}')"
python3 -I -c 'import sys,json;d=json.load(sys.stdin);v=d["value"];assert len(v)==1 and v[0]["text"]=="ship lakebed" and v[0]["done"] is False, v' <<<"$LIST" \
  && ok "list query read the row back from shadw storage" || bad "list query ($LIST)"

CNT="$(post query count '{"ownerId":"witness"}')"
python3 -I -c 'import sys,json;assert json.load(sys.stdin)["value"]==1' <<<"$CNT" \
  && ok "count query measured 1" || bad "count query ($CNT)"

TOG="$(post mutation toggle "{\"id\":\"$ID\"}")"
python3 -I -c 'import sys,json;d=json.load(sys.stdin);assert d["value"]["done"] is True' <<<"$TOG" \
  && ok "toggle mutation persisted done=true" || bad "toggle ($TOG)"

echo "==> storage binding: upload -> read back -> list"
UP="$(curl -s -m8 -X POST "$APP/api/files" --data-binary 'lakebed-storage-roundtrip')"
FKEY="$(python3 -I -c 'import sys,json;d=json.load(sys.stdin);assert d["size"]==25,d;print(d["key"])' <<<"$UP")"
[ -n "$FKEY" ] && ok "upload wrote and re-read 25 bytes" || bad "upload ($UP)"
BACK="$(curl -s -m8 "$APP/api/files/$FKEY")"
[ "$BACK" = "lakebed-storage-roundtrip" ] && ok "GET /api/files/:key round trip exact" || bad "file read ($BACK)"

echo "==> delete via the sanctioned console path"
DEL="$(post mutation remove "{\"id\":\"$ID\"}")"
python3 -I -c 'import sys,json;d=json.load(sys.stdin);assert d["value"]["deleted"] is True,d' <<<"$DEL" \
  && ok "remove mutation deleted the row via console" || bad "remove ($DEL)"
AFTER="$(post query count '{"ownerId":"witness"}')"
python3 -I -c 'import sys,json;assert json.load(sys.stdin)["value"]==0' <<<"$AFTER" \
  && ok "count is 0 after delete (no phantom row)" || bad "post-delete count ($AFTER)"

echo "==> static client + typed refusals"
HTML="$(curl -s -m5 "$APP/")"
printf '%s' "$HTML" | grep -q 'Lakebed Todo' && ok "static client served" || bad "static client"
CLIENTJS="$(curl -s -m5 "$APP/client/index.mjs")"
printf '%s' "$CLIENTJS" | grep -q 'export function createClient' && ok "client module served from /client/" || bad "client module"
BAD="$(post mutation add '{"text":"","ownerId":"witness"}')"
python3 -I -c 'import sys,json;d=json.load(sys.stdin);assert d["ok"] is False' <<<"$BAD" \
  && ok "empty text refused with a typed error" || bad "empty text ($BAD)"
SECRET_LEAK="$(post query list '{"ownerId":"witness"}')"
printf '%s' "$SECRET_LEAK" | grep -q 'hive_' && bad "credential leaked into a response" || ok "no credential in responses"

echo "==> RESULT pass=$PASS fail=$FAIL"
[ "$FAIL" -eq 0 ]
