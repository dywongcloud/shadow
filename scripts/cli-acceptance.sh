#!/usr/bin/env bash
# Acceptance / system test: the REAL `shadw` CLI against a REAL hive-cloud node,
# end to end — through the actual control API, in-memory stores, and the on-disk
# persistence layer (the "DB"). Hermetic: a throwaway node on isolated ports with
# a temp data dir; deploys a local file:// repo (no network); cleans up on exit.
#
# Usage:  ./scripts/cli-acceptance.sh
set -uo pipefail
cd "$(dirname "$0")/.."

ADMIN=8899; PUBLIC=8898; DNS=5390; TLS=8490
API="http://127.0.0.1:$ADMIN"
DATA="$(mktemp -d)"
REPO="$(mktemp -d)/acc-repo"
HOMEDIR="$(mktemp -d)"     # isolated ~/.shadw for `shadw auth login`
SHADW=./target/debug/shadw
NODE=./target/debug/hive-cloud
PASS=0; FAIL=0; NODE_PID=""

cleanup() { [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null; rm -rf "$DATA" "$REPO" "$HOMEDIR"; }
trap cleanup EXIT

ok()   { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }
# assert <description> <command...>  → command must exit 0
assert() { local d="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d"; fi; }
# assert_out <description> <expected-substr> <command...>  → stdout must contain substr
assert_out() { local d="$1" want="$2"; shift 2; local o; o="$("$@" 2>&1)"; if echo "$o" | grep -qF "$want"; then ok "$d"; else bad "$d (wanted '$want', got: $(echo "$o" | head -1))"; fi; }
# assert_fail <description> <command...>  → command must exit NON-zero
assert_fail() { local d="$1"; shift; if "$@" >/dev/null 2>&1; then bad "$d (expected failure)"; else ok "$d"; fi; }

echo "==> Building hive-cloud + shadw"
cargo build -q -p hive-cloud -p shadw-cli || { echo "build failed"; exit 1; }

start_node() {
  HIVE_DATA="$DATA" HIVE_DNS_ADDR="127.0.0.1:$DNS" HIVE_TLS_ADDR="127.0.0.1:$TLS" RUST_LOG=warn \
    "$NODE" --name acc-node --listen "127.0.0.1:$PUBLIC" --admin "127.0.0.1:$ADMIN" > "$DATA/node.log" 2>&1 &
  NODE_PID=$!
  for _ in $(seq 1 40); do curl -s -m1 "$API/healthz" >/dev/null 2>&1 && return 0; sleep 0.5; done
  echo "node did not come up; log:"; tail -20 "$DATA/node.log"; exit 1
}

echo "==> Starting throwaway node ($API, data=$DATA)"
start_node

# A local static repo to deploy (hermetic — no network).
mkdir -p "$REPO"; printf '<!doctype html><h1>acc</h1>' > "$REPO/index.html"
( cd "$REPO" && git init -q -b main && git config user.email a@a && git config user.name a && git add -A && git commit -qm init )

# Issue an API key via the API (dev: unauth, team=personal), then log the CLI in.
KEY="$(curl -s -m5 -X POST "$API/v1/apikeys" -H 'content-type: application/json' -d '{"name":"acc","role":"owner"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')"
[ -n "$KEY" ] || { echo "failed to issue API key"; exit 1; }
export HOME="$HOMEDIR"

echo "==> Auth"
assert_out "auth login saves config + verifies"        "Logged in"      "$SHADW" auth login --token "$KEY" --api "$API"
assert_out "auth whoami reports the key's team"         "personal"       "$SHADW" auth whoami
assert_out "keys list shows the issued key (uses saved config)" "acc"    "$SHADW" keys list

echo "==> Observability (read-only)"
assert     "obs overview"   "$SHADW" obs overview --json
assert_out "obs nodes lists the node"  "acc-node"  "$SHADW" obs nodes

echo "==> Deploy (clone → build → ready) via the real build pipeline"
assert_out "deploy --follow reaches ready" "ready"  "$SHADW" deploy "file://$REPO" --project accproj --follow
assert_out "deps list shows the project"   "accproj" "$SHADW" deps list
# Compute multi-tenancy: the deployment (and the real cells it spawns) is tagged
# with its owning tenant, threaded deploy → Fluid pool → CellSpec.
DTEN="$(curl -s -m5 "$API/deployments" -H "authorization: Bearer $KEY" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(next((x.get("tenant","") for x in d if x.get("project")=="accproj"),"MISSING"))')"
[ "$DTEN" = "personal" ] && ok "deployment accproj is tenant-tagged ($DTEN)" || bad "deployment tenant tag (got '$DTEN')"

echo "==> Project env CRUD (encrypted-at-rest store)"
assert     "env set (sensitive)"  "$SHADW" projects env set accproj API_TOKEN s3cr3t --sensitive
assert_out "env list shows the key" "API_TOKEN" "$SHADW" projects env list accproj
assert     "env rm"               "$SHADW" projects env rm accproj API_TOKEN

echo "==> Domains + DNS records CRUD (DomainStore)"
"$SHADW" projects domain accproj acc.example >/dev/null 2>&1 || true
RID="$("$SHADW" domains record add acc.example --type A --name www --value 1.2.3.4 --json | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')"
[ -n "$RID" ] && ok "domains record add returns id" || bad "domains record add"
assert_out "domains record list shows it" "1.2.3.4" "$SHADW" domains record list acc.example
assert     "domains record rm"            "$SHADW" domains record rm acc.example "$RID"

echo "==> API-key lifecycle (create → revoke round-trip)"
NEW_ID="$("$SHADW" keys create rt --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')"
[ -n "$NEW_ID" ] && ok "keys create returns id+token" || bad "keys create"
assert     "keys revoke"  "$SHADW" keys revoke "$NEW_ID"

echo "==> Multi-tenant isolation (cross-tenant access must be DENIED)"
# Mint two API keys scoped to two different teams (team comes from x-hive-team
# when creating the key; thereafter the key's bearer token carries that team).
mint_key() { curl -s -m5 -X POST "$API/v1/apikeys" -H "x-hive-team: $1" -H 'content-type: application/json' -d '{"name":"'$1'","role":"owner"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])'; }
hcode() { curl -s -m5 -o /dev/null -w '%{http_code}' "$@"; }
KEY_A="$(mint_key alpha)"; KEY_B="$(mint_key beta)"
[ -n "$KEY_A" ] && [ -n "$KEY_B" ] && ok "minted per-team API keys (alpha, beta)" || bad "mint per-team keys"

# --- Databases: alpha owns a DB with secret credentials; beta must not see it. ---
DBID="$(curl -s -m5 -X POST "$API/v1/databases" -H "authorization: Bearer $KEY_A" -H 'content-type: application/json' -d '{"name":"secretdb","project":"aproj","kind":"redis"}' | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')"
[ -n "$DBID" ] && ok "alpha provisioned database $DBID" || bad "alpha provision database"
[ "$(hcode "$API/v1/databases/$DBID" -H "authorization: Bearer $KEY_A")" = "200" ] && ok "alpha reads its OWN database (200)" || bad "alpha cannot read own db"
[ "$(hcode "$API/v1/databases/$DBID" -H "authorization: Bearer $KEY_B")" = "404" ] && ok "beta BLOCKED from alpha's database (404)" || bad "beta read alpha's db (LEAK)"
[ "$(hcode "$API/v1/databases/$DBID/credentials" -H "authorization: Bearer $KEY_B")" = "404" ] && ok "beta BLOCKED from alpha's DB credentials (404)" || bad "beta read alpha's credentials (LEAK)"
[ "$(hcode -X DELETE "$API/v1/databases/$DBID" -H "authorization: Bearer $KEY_B")" = "404" ] && ok "beta BLOCKED from deleting alpha's database (404)" || bad "beta deleted alpha's db"
[ "$(hcode "$API/v1/databases/$DBID" -H "authorization: Bearer $KEY_A")" = "200" ] && ok "alpha's database SURVIVED beta's delete attempt" || bad "db lost to cross-tenant delete"
[ "$(hcode -X DELETE "$API/v1/databases/$DBID" -H "authorization: Bearer $KEY_A")" = "200" ] && ok "alpha deletes its OWN database (200)" || bad "alpha self-delete failed"

# --- Webhooks: tenant-scoped list + delete (validates the new Webhook.team field). ---
WHID="$(curl -s -m5 -X POST "$API/v1/webhooks" -H "authorization: Bearer $KEY_A" -H 'content-type: application/json' -d '{"url":"https://alpha.example/hook"}' | python3 -c 'import sys,json;d=json.load(sys.stdin);print((d[0] if isinstance(d,list) else d).get("id",""))')"
[ -n "$WHID" ] && ok "alpha created webhook $WHID" || bad "alpha create webhook"
assert_out "alpha's webhook list shows it"           "$WHID" curl -s -m5 "$API/v1/webhooks" -H "authorization: Bearer $KEY_A"
o="$(curl -s -m5 "$API/v1/webhooks" -H "authorization: Bearer $KEY_B")"; echo "$o" | grep -qF "$WHID" && bad "beta sees alpha's webhook (LEAK)" || ok "beta's webhook list EXCLUDES alpha's webhook"
[ "$(hcode -X DELETE "$API/v1/webhooks/$WHID" -H "authorization: Bearer $KEY_B")" = "404" ] && ok "beta BLOCKED from deleting alpha's webhook (404)" || bad "beta deleted alpha's webhook"
[ "$(hcode -X DELETE "$API/v1/webhooks/$WHID" -H "authorization: Bearer $KEY_A")" = "200" ] && ok "alpha deletes its OWN webhook (200)" || bad "alpha webhook self-delete failed"

echo "==> Error handling + generic passthrough"
assert_out "api passthrough GET /v1/auth" "enforced" "$SHADW" api GET /v1/auth
assert_fail "unknown endpoint exits non-zero" "$SHADW" api GET /v1/does-not-exist

echo "==> Persistence end-to-end: restart node, state survives (on-disk DB layer)"
kill "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null; NODE_PID=""
start_node
assert_out "API key persisted + still verifies after restart" "acc"     "$SHADW" keys list
assert_out "deployment persisted + still listed after restart" "accproj" "$SHADW" deps list

echo "==> Cleanup-path commands"
assert "delete project" "$SHADW" projects delete accproj
assert_out "auth logout" "Logged out" "$SHADW" auth logout

echo
echo "================  $PASS passed, $FAIL failed  ================"
[ "$FAIL" -eq 0 ]
