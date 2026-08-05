# Hive Cloud — Dashboard

A Vercel-style control plane UI (Next.js + TypeScript + Tailwind + Tremor) for a
`hive-cloud` node. Dark/Geist aesthetic, sidebar nav, live-polling pages.

## Run

```bash
# 1) Start a node (from the repo root)
cargo run -p hive-cloud -- --region sfo1 --name node-a   # public :8787, admin :8786

# 2) Start the dashboard (this dir)
npm install
npm run dev            # http://localhost:3000
```

The dashboard proxies `/cloud/*` → the node's admin API (`HIVE_ADMIN`, default
`http://127.0.0.1:8786`) via a Next.js rewrite, so there's no CORS setup.

## Local full-page testing (JWT path)

The stock local node runs **unenforced** (no `HIVE_JWT_SECRET`), so mutations
pass and the tenant comes from the dev `x-hive-team` header — but none of the
production auth machinery (mint → `hive_jwt` cookie → JWT-claim tenant,
401-remint, anonymous-tenant fallback) is exercised, and pointing the UI at an
**enforced** backend without Clerk signs everything in as the anonymous
namespace (pages render, every list is empty). The dev-mint closes that gap:

```bash
# 1) Enforced local node
HIVE_JWT_SECRET=devsecret HIVE_INTERNAL_TOKEN=devinternal HIVE_DATA=/tmp/hive-dev \
  ./target/debug/hive-cloud --name node-a --listen 127.0.0.1:8787 --admin 127.0.0.1:8786

# 2) Seed something under "personal" (mutations 401 without a token — that 401
#    is itself the enforcement witness)
TOK=$(curl -sX POST 127.0.0.1:8786/v1/token -H 'x-hive-internal: devinternal' \
  -H 'content-type: application/json' \
  -d '{"sub":"local-dev","tenant":"personal","role":"owner","email":""}' | jq -r .token)
curl -sX POST 127.0.0.1:8786/v1/deploy/zip -H "authorization: Bearer $TOK" \
  -H 'content-type: application/zip' -H "x-hive-deploy-meta: $(echo -n '{"project":"seed"}' | base64)" \
  --data-binary @app.zip

# 3) Dashboard against the enforced node, with BOTH Clerk keys fully unset
cd ui && HIVE_AUTH_BYPASS=1 HIVE_ADMIN=http://127.0.0.1:8786 \
  HIVE_INTERNAL_TOKEN=devinternal NEXT_PUBLIC_HIVE_DEV_MINT=1 npm run dev
```

The dashboard renders signed-out-free (the `HIVE_AUTH_BYPASS` hatch) AND its
`/cloud` calls are authenticated: `/api/token`'s dev branch mints a local JWT
for the requested team and sets the same httpOnly `hive_jwt` cookie the Clerk
flow sets. The contrast is the proof — the same setup without
`NEXT_PUBLIC_HIVE_DEV_MINT=1` renders every list empty (anonymous tenant).
Residual boundary: Clerk's own sign-in/org UI is never exercised locally, and
the dev mint asserts any tenant, so it is local-node-only — never point it at
the fleet with the real `HIVE_INTERNAL_TOKEN`. Env-skew rule: blanking a
previously-set `NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY` needs `rm -rf ui/.next`
(stale chunks inline the old key); fully unset from the start never skews.

## Pages

| Page | Talks to |
| --- | --- |
| Overview | `/v1/overview`, `/v1/logs` |
| Deployments | `/deployments` |
| Functions | `/v1/functions` |
| Regions | `/v1/nodes` |
| Firewall | `/v1/waf`, `/v1/bot` |
| Cron Jobs | `/v1/cron` |
| Workflows | `/v1/workflows`, `/v1/workflows/runs` |
| Observability | `/v1/logs` |
| Sandbox | `/v1/sandbox` |

## Multi-node

Point at another MacBook's node by setting `HIVE_ADMIN=http://<ip>:8786`, or mesh
nodes with `hive-cloud --peer http://<ip>:8786` and they'll appear under Regions.
