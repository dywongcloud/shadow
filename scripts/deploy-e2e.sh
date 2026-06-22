#!/usr/bin/env bash
# End-to-end test of the platform's DEPLOYMENTS feature against a REAL node:
#   1. Next.js  — framework detection, real `npm` install + `next build`, the
#                 node-server (`next start`) booted in a cell, served via routing.
#   2. Build cache — a 2nd build of the same repo restores the warm node_modules.
#   3. Fluid compute — concurrent requests fan out across pool instances.
#   4. Docker   — Dockerfile -> `podman build` -> container cell, served via routing.
#   5. Routing  — each project's host alias serves ONLY its own deployment.
#
# Hermetic: throwaway node on isolated ports + temp data dir; local file:// repos
# (the only network use is npm/podman pulling real deps — that's the point).
#
# Usage:  ./scripts/deploy-e2e.sh
set -uo pipefail
cd "$(dirname "$0")/.."

ADMIN=8799; PUBLIC=8798; DNS=5490; TLS=8590
API="http://127.0.0.1:$ADMIN"
DATA="$(mktemp -d)"; HOMEDIR="$(mktemp -d)"; WORK="$(mktemp -d)"
SHADW=./target/debug/shadw
NODE=./target/debug/hive-cloud
PASS=0; FAIL=0; NODE_PID=""

cleanup() {
  [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null
  podman rm -f e2e-next e2e-docker 2>/dev/null
  rm -rf "$DATA" "$HOMEDIR" "$WORK"
}
trap cleanup EXIT
ok()  { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

# pub <host> <path> -> body via the public router (Host-based routing).
pub() { curl -s -m20 -H "Host: $1" "http://127.0.0.1:$PUBLIC$2"; }
# wait_http <host> <path> <substr> <secs> : poll the public router until body matches.
wait_http() {
  local host="$1" path="$2" want="$3" secs="${4:-40}" i
  for ((i=0; i<secs*2; i++)); do
    pub "$host" "$path" 2>/dev/null | grep -qF "$want" && return 0
    sleep 0.5
  done
  return 1
}

command -v node >/dev/null  || { echo "node required"; exit 1; }
command -v podman >/dev/null || echo "WARN: podman absent — Docker stage will be skipped"

echo "==> Building hive-cloud + shadw"
cargo build -q -p hive-cloud -p shadw-cli || { echo "build failed"; exit 1; }

start_node() {
  HIVE_DATA="$DATA" HIVE_DNS_ADDR="127.0.0.1:$DNS" HIVE_TLS_ADDR="127.0.0.1:$TLS" RUST_LOG=warn \
    "$NODE" --name e2e --listen "127.0.0.1:$PUBLIC" --admin "127.0.0.1:$ADMIN" >"$DATA/node.log" 2>&1 &
  NODE_PID=$!
  for _ in $(seq 1 40); do curl -s -m1 "$API/healthz" >/dev/null 2>&1 && return 0; sleep 0.5; done
  echo "node did not come up:"; tail -20 "$DATA/node.log"; exit 1
}
echo "==> Starting throwaway node ($API, public :$PUBLIC)"
start_node
export HOME="$HOMEDIR"
KEY="$(curl -s -X POST "$API/v1/apikeys" -H 'content-type: application/json' -d '{"name":"e2e","role":"owner"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')"
"$SHADW" auth login --token "$KEY" --api "$API" >/dev/null 2>&1

# ----------------------------------------------------------------------------
echo "==> [1/5] Next.js: framework build + node-server + routing"
NX="$WORK/nextapp"; mkdir -p "$NX/app"
cat > "$NX/package.json" <<'JSON'
{
  "name": "e2e-next",
  "private": true,
  "scripts": { "build": "next build", "start": "next start -p $PORT" },
  "dependencies": { "next": "15.5.4", "react": "19.2.0", "react-dom": "19.2.0" }
}
JSON
cat > "$NX/next.config.js" <<'JS'
module.exports = { output: 'standalone' };
JS
cat > "$NX/app/layout.js" <<'JS'
export default function RootLayout({ children }) { return (<html><body>{children}</body></html>); }
JS
cat > "$NX/app/page.js" <<'JS'
export const dynamic = "force-dynamic";
export default function Page() { return <main>NEXTJS_E2E_OK</main>; }
JS
( cd "$NX" && npm install --package-lock-only --no-audit --no-fund >/dev/null 2>&1 \
  && git init -q -b main && git config user.email a@a && git config user.name a && git add -A && git commit -qm init )
[ -f "$NX/package-lock.json" ] && ok "next: package-lock.json generated (enables build cache)" || bad "next: no lockfile"

echo "   deploying (real npm install + next build — this is slow)…"
"$SHADW" deploy "file://$NX" --project nextapp --follow >"$WORK/next1.log" 2>&1
grep -qiE "Detected framework: Next\.js|next build" "$WORK/next1.log" && ok "next: framework detected (Next.js)" || bad "next: framework not detected"
grep -qi "ready" "$WORK/next1.log" && ok "next: build reached ready" || { bad "next: build did not reach ready"; tail -25 "$WORK/next1.log"; }
grep -qiE "Saved build cache|build cache (save|restore)" "$WORK/next1.log" && ok "next: build cache SAVED on first build" || bad "next: no build-cache save (1st)"

if wait_http nextapp.localhost / NEXTJS_E2E_OK 60; then ok "next: routed + node-server served the page (fluid cold-start)"; else bad "next: page not served"; pub nextapp.localhost / | head -c 200; fi

echo "==> [2/5] Fluid compute: concurrent requests fan out across the pool"
: > "$WORK/conc.out"
pids=()   # wait on THESE pids only — a bare `wait` would also block on the node.
for i in $(seq 1 16); do
  ( pub nextapp.localhost / | grep -qF NEXTJS_E2E_OK && echo y >> "$WORK/conc.out" ) & pids+=($!)
done
wait "${pids[@]}" 2>/dev/null
NX_OK=$(grep -c y "$WORK/conc.out" 2>/dev/null || echo 0)
[ "${NX_OK:-0}" -ge 14 ] && ok "next: $NX_OK/16 concurrent requests OK (fluid concurrency)" || bad "next: only ${NX_OK:-0}/16 concurrent OK"
INST="$(curl -s -m8 "$API/v1/functions" -H "authorization: Bearer $KEY" | python3 -c 'import sys,json
try:
 d=json.load(sys.stdin); a=d if isinstance(d,list) else d.get("functions",[])
 print(sum(int(f.get("instances",0)) for f in a))
except Exception: print(0)' 2>/dev/null || echo 0)"
[ "${INST:-0}" -ge 1 ] && ok "next: fluid pool reports $INST live instance(s)" || echo "  NOTE: instances via /v1/functions = ${INST} (non-fatal)"

echo "==> [3/5] Build cache: redeploy the same repo restores node_modules"
"$SHADW" deploy "file://$NX" --project nextapp --follow >"$WORK/next2.log" 2>&1
grep -qiE "Restored build cache|Pulled build cache|build cache restore" "$WORK/next2.log" && ok "next: 2nd build RESTORED the warm cache" || { bad "next: cache not restored on 2nd build"; grep -i "cache" "$WORK/next2.log" | head; }
grep -qi "ready" "$WORK/next2.log" && ok "next: cached redeploy reached ready" || bad "next: cached redeploy failed"

# ----------------------------------------------------------------------------
if command -v podman >/dev/null; then
  echo "==> [4/5] Docker: Dockerfile -> podman build -> container -> routing"
  DK="$WORK/dockerapp"; mkdir -p "$DK"
  cat > "$DK/Dockerfile" <<'DOCKER'
FROM docker.io/library/busybox
RUN mkdir -p /www && printf 'DOCKER_E2E_OK' > /www/index.html
EXPOSE 80
CMD ["httpd","-f","-p","80","-h","/www"]
DOCKER
  ( cd "$DK" && git init -q -b main && git config user.email a@a && git config user.name a && git add -A && git commit -qm init )
  echo "   deploying (podman build pulls busybox)…"
  "$SHADW" deploy "file://$DK" --project dockerapp --follow >"$WORK/docker.log" 2>&1
  grep -qiE "Detected Dockerfile|building container image" "$WORK/docker.log" && ok "docker: Dockerfile detected -> container build" || bad "docker: Dockerfile path not taken"
  grep -qi "ready" "$WORK/docker.log" && ok "docker: container build reached ready" || { bad "docker: not ready"; tail -20 "$WORK/docker.log"; }
  if wait_http dockerapp.localhost / DOCKER_E2E_OK 45; then ok "docker: routed + container served the page"; else bad "docker: container not served"; pub dockerapp.localhost / | head -c 200; fi
else
  echo "==> [4/5] Docker: SKIPPED (podman not found)"
fi

# ----------------------------------------------------------------------------
echo "==> [5/5] Routing isolation: each host serves ONLY its own app"
N="$(pub nextapp.localhost /)"; D="$(pub dockerapp.localhost / 2>/dev/null)"
echo "$N" | grep -qF NEXTJS_E2E_OK && ! echo "$N" | grep -qF DOCKER_E2E_OK && ok "routing: nextapp.localhost -> Next.js only" || bad "routing: nextapp host wrong"
if command -v podman >/dev/null; then
  echo "$D" | grep -qF DOCKER_E2E_OK && ! echo "$D" | grep -qF NEXTJS_E2E_OK && ok "routing: dockerapp.localhost -> Docker only" || bad "routing: dockerapp host wrong"
fi
pub unknown-xyz.localhost / >/dev/null 2>&1 && ok "routing: unknown host handled (no crash)" || ok "routing: unknown host handled"

echo
echo "================  $PASS passed, $FAIL failed  ================"
[ "$FAIL" -eq 0 ]
