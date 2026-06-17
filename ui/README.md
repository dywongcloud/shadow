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
