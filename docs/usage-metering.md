# Usage metering and the profitable rate card

Status: landed in the tree. Charging is OFF (shadow mode) until the operator sets
`HIVE_BILLING_METER_RESOURCES=1`. History lives in `CHANGELOG.md`.

## The model

A SKU is sold only when its public price covers its cost of goods sold (COGS) with margin to spare.
`billing::RATE_CARD` is the ONE public price list and the single source of truth for metered
pricing; `billing::COST_CARD` is the ONE internal cost basis. `RATE_CARD.margin_report()` emits one
`SkuMargin { sku, price_cents, cogs_cents, gross_margin_bps }` per priced SKU, and
`RATE_CARD.assert_profitable()` refuses a card where any one falls below
`MIN_GROSS_MARGIN_BPS = 6000` (60%).

Two endpoints, two audiences:

- `GET /v1/usage/summary` is tenant-scoped and owner-proxied: it returns the caller's OWN cumulative
  usage plus the PUBLIC rate card. It NEVER carries COGS, another tenant's data, or the margin
  report. It is a GET over owner-held state, so it needs the owner proxy (`build_get`,
  `fetch_from_host`), never a node-local read.
- `GET /v1/admin/usage/economics` is operator-only: `COST_CARD`, `margin_report()`,
  `MIN_GROSS_MARGIN_BPS`, the profitable status and `resource_charging_enabled()`.

Summary keys: `usage` (`UsageTotals`), `rate_card`, `included_cents`, `overage_ceiling_cents`.
Economics keys: `cost_card`, `margin_report`, `min_gross_margin_bps`, `profitable`,
`unprofitable_skus`, `resource_charging_enabled`. `usage_economics` hard-gates on
`require_operator` (`platform_admin`); the `roles::RULES` admin floor alone never opens COGS.

COGS never crosses the tenant boundary: a tenant sees what it used and what it costs to buy, not
what it costs the platform to serve. `GET /v1/gitops/projects?view=summary` stays byte-identical
and carries no usage data.

### Why 60%

`PlanSpec.included_cents == price_cents` on both paid plans. The subscription's gross margin IS the
metered margin: there is no separate base fee to absorb a loss-making SKU, so one SKU below cost
makes the whole plan below cost. 60% leaves 40% for the costs a per-SKU COGS figure does not
contain: idle host capacity, power, transit contracts, storage replication, support and payment
fees. A price that merely beats COGS is still a loss once those are counted. The floor is a gate,
not a target: `assert_profitable()` fails the card, it never raises a price by itself.

`PlanSpec.overage_ceiling_cents` bounds exposure above the included credit (Hobby 0, Pro 10000,
Enterprise 1000000). `BillingStore::may_consume(tenant, projected_cents)` is the typed refusal
that enforces it; a plan is bought or granted, never self-assigned.

## SKU table

Prices are `billing::RATE_CARD` and COGS is `billing::COST_CARD`, both in cents per billed unit.
The COGS column IS the measured card; margin is `(price - cogs) / price` in basis points, reported
by `RATE_CARD.margin_report()`. A zero-price SKU is a counter, not a sale: `assert_profitable()`
SKIPS it rather than dividing by zero. Public references are the derivation basis for the COGS
inputs, listed below.

| SKU | Unit | Price (c) | COGS (c) | Margin (bps) | Public reference |
| --- | --- | --- | --- | --- | --- |
| `active_cpu_hr_cents` | CPU-hour | 12.8 | 0.3042 | 9762 | Fly.io |
| `mem_gb_hr_cents` | GB-hour | 1.06 | 0.0208 | 9803 | Fly.io |
| `requests_per_million_cents` | million requests | 200.0 | 20.0 | 9000 | Bunny |
| `waf_per_million_cents` | million blocked | 60.0 | 6.0 | 9000 | Bunny |
| `gpu_hr_cents` | GPU-hour | 60.0 | 20.0 | 6666 | no public GPU price in this reference set |
| `browser_per_million_cents` | million donated-node requests | 0.0 | 0.0 | SKIPPED | not a sale (donated node, zero platform cost) |
| `db_row_ops_per_million_cents` | million row ops | 20.0 | 7.345 | 6327 | Turso, Neon, Cloudflare D1 |
| `db_storage_gb_month_cents` | GB-month | 15.0 | 1.5 | 9000 | Backblaze B2 |
| `object_storage_gb_month_cents` | GB-month | 1.5 | 0.5 | 6666 | Backblaze B2 |
| `ingress_gb_cents` | GB | 0.2 | 0.05 | 7500 | Fly.io, B2, Bunny |
| `egress_gb_cents` | GB | 1.0 | 0.2 | 8000 | Bunny, Fly.io, B2 |

Every priced SKU clears 6000 bps. `db_row_ops_per_million_cents` (6327) is the tightest, then
`gpu_hr_cents` and `object_storage_gb_month_cents` (6666); a COGS drift in one of those breaks
`assert_profitable()` first.

The egress row is the one that can invert on a price cut. Fly lists $0.02/GB and Backblaze B2
$0.01/GB retail, while the fleet buys transit at Bunny-bulk-class rates ($0.002/GB), which is where
the 0.2 c/GB COGS comes from. `COST_CARD` records the contracted rate, not a retail list price; a
price cut made without re-measuring that rate is exactly the failure `assert_profitable()` catches.

### Public references

Conversions use 720 hours per 30-day month and 1 GB = 1e9 bytes. These are the market prices and
commodity inputs behind `COST_CARD`, fetched 2026-11-07:

- Fly.io: https://fly.io/docs/about/pricing/ ($2.19/30d shared CPU, $0.15/GB-mo volume,
  $0.02/GB egress)
- Backblaze B2: https://www.backblaze.com/cloud-storage/pricing and
  https://www.backblaze.com/cloud-storage/transaction-pricing ($0.005/GB-mo storage,
  $0.01/GB egress)
- Bunny: https://bunny.net/pricing/ (bulk $0.002/GB)
- Turso: https://turso.tech/pricing (Pro $0.80/B reads, $0.80/M writes)
- Neon: https://neon.tech/pricing.md
- Cloudflare D1: https://developers.cloudflare.com/d1/platform/pricing/
- Cloudflare R2: https://developers.cloudflare.com/r2/pricing/

`COST_CARD` is operator-measured, not third-party audited. `margin_report()` is only as honest as
the numbers in it; `assert_profitable()` proves arithmetic, not procurement.

## Single-writer invariant

Metering has ONE writer per tenant per tick. `may_act(cloud, BillingMeter)` is the same
leader-only gate every other job asks: owner AND fresh view AND not isolated, held for the job's
tenure. The elected owner alone runs `meter_usage`; every other node HOLDS, it never writes a
charge. An owner that cannot act is still owner everywhere, so the correct repair is to restore
transport/voters, never to weaken the gate.

Each node keeps its OWN node-local cumulative counters (CPU ms, memory GB-hr milli, requests,
blocked, GPU ms, browser requests, db row ops, db/object storage byte-ms, ingress/egress bytes).
The owner reads those cumulative counters and charges the DELTA since the last successfully
charged watermark, ONCE per tenant per tick. Charging a cumulative total double-charges; charging
from two nodes double-charges. The watermark advances only after the charge is durably recorded,
so a crash replays the same delta rather than losing it. `may_consume(tenant, projected_cents)`
is checked against the projected delta BEFORE the charge, not after.

## serde(default) snapshot rule

The five new `billing::UsageTotals` fields (`db_row_ops`, `db_storage_byte_ms`,
`object_storage_byte_ms`, `ingress_bytes`, `egress_bytes`) are `#[serde(default)]`: a snapshot
written before the extension loads with those counters as ZERO, never as a deserialize error
(silent total state loss in `persist::load`). The mirror holds on the way out too: a serde-REQUIRED
key is never removed from a persisted struct in one release. The dashboard keeps those five fields
optional for the same reason: a node that predates them answers without them and the view renders
absent values as absent, never as a fabricated zero. Usage totals are counters, never money, so
adding a field is additive and never rewrites history.

## Anti-abuse rule

No byte path is both unmetered and unbounded. Every tenant-controlled path that can move or hold
bytes (object storage PUT/GET, database row ops, managed SQLite/CRR writes, ingress, egress,
build artifacts and container images) has either a metering hook or a hard bound. The admission
check is `BillingStore::may_consume(tenant, projected_cents)` and the ceiling is
`PlanSpec.overage_ceiling_cents`; a projected charge beyond it is a TYPED refusal (402 payment
required / 429 throttled) returned BEFORE the bytes land, with the whole batch rolled back rather
than truncated or evicted mid-stream. Refusing after the write is not metering, it is a bill the
tenant cannot pay. A 402/429 is a measurement boundary, never an empty 200: the UI treats a
missing or anonymous summary as NO DATA and seeds nothing.

## Shadow mode and the operator enable procedure

`resource_charging_enabled()` is OFF by default. It is true only when BOTH an operator sets
`HIVE_BILLING_METER_RESOURCES=1` (or `true`) AND every priced SKU on `RATE_CARD` clears
`MIN_GROSS_MARGIN_BPS` against `COST_CARD`; a missing or unrecognised opt-in, or a price below
COGS, keeps charging OFF and fails closed. With it OFF the meter still measures, records and
reports, but writes NO charge. Shadow mode is the default because a new SKU first proves its
counters against live traffic.

Enabling is a production change: the operator's word first, then `ansible/AGENTS.md`.

1. Roll the code fleet-wide with `HIVE_BILLING_METER_RESOURCES` unset. Prove the RUNNING binary,
   never a Rust format literal.
2. Let at least one FULL billing period run in shadow. Read `GET /v1/admin/usage/economics`:
   `profitable` must be true and EVERY `SkuMargin.gross_margin_bps` at least 6000.
3. Reconcile shadow deltas against the node-local cumulative counters and against
   `GET /v1/usage/summary`. A counter that moves without a matching delta, or a delta with no
   counter, is a blocker; fix it before enabling.
4. Set `HIVE_BILLING_METER_RESOURCES=1` on EVERY node in the same roll (unit drop-in). A partial
   fleet charges from a subset of writers and silently under-bills; never enable on one node to
   "try it".
5. Re-check `/v1/admin/usage/economics` after the roll: `resource_charging_enabled` true on every
   node, `profitable` still true. Watch the first charge tick for a doubled delta.
6. To roll back, unset the flag on every node; measured counters and the charged watermark survive,
   so no usage is lost and no charge is replayed.

## Honest residuals (what is NOT yet metered)

- Build minutes, build-cache tarballs, container/rootfs image storage, GitOps checkout disk, log
  retention and deployment snapshots are NOT metered. They are real cost with no SKU.
- `browser_per_million_cents` is 0.0 by design and is not a sale. Cross-tenant serving by a donated
  browser node is not attributed to a tenant at all; only the owner's own app is free.
- GPU is `gpu_ms` wall-time for an instance that advertises a GPU. There is no per-model,
  per-VRAM or fractional-GPU accounting.
- `mem_gb_hr_milli` is PROVISIONED memory, not peak RSS. Idle-but-provisioned memory is charged by
  design; a tenant cannot see its own peak.
- Inter-node mesh/gossip traffic, store-sync replication, GuardianDB restore and control-plane
  health probes are platform overhead, not tenant-metered ingress/egress.
- Egress is measured at the gateway, not per origin. A peer-cache hit may be counted once rather
  than once per hop.
- `db_row_ops` counts at ONE engine chokepoint; nested triggers, CRR clock-table writes and
  background index-walker writes are not separately attributed to a tenant.
- `blocked` counts WAF blocks, not inspected requests that passed; passed inspections have no SKU.
- `may_consume` is a periodic projection, not a real-time per-request quota. A burst inside one
  tick can pass the ceiling before the next typed refusal. There is no per-request hard cut.
- Zero-price SKUs are skipped by `assert_profitable()`. If one is ever priced, its COGS must be
  recorded in `COST_CARD` in the SAME release.
- `/v1/usage/summary` is owner-proxied; a stranded owner path answers empty. The UI shows an
  explicit empty state, never zeros, but the data is not available off-owner.

Dashboard: `ui/lib/api.ts` carries the 11-SKU `RateCard` (five new fields optional), `UsageTotals`,
`SkuMargin`, `CostCard`, `UsageSummary`, `UsageEconomics` and `getUsageSummary()`;
`ui/app/usage/usage-view.tsx` renders the five storage/network SKUs. The existing compute
section's numbers are untouched.
