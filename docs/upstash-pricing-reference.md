# Upstash public pricing reference

Fetched 2026-11-11 (UTC) through gm's `fetch` verb. This document covers the shadw sandbox and
managed data products. The six pages below are a PUBLIC REFERENCE: Upstash's published numbers and
plan labels are NOT shadw's prices. The only metered prices this platform has are the `RateCard`
fields in `crates/hive-cloud/src/billing.rs`; none of the Upstash figures below are `RATE_CARD`
values, and this document sets no shadw price.

## Fetch integrity

All six fetches returned HTTP 200. None refused. The fetch verb reported `truncated: false` on
each, and the full converted markdown was read back from each dispatch's result (4,747 to 11,528
characters); an initial compact reply preview was capped, so the full `content` field was read
with `gm_result` and reported `complete: true`. No missing text is filled in below. Converted
markdown drops some decorative headings (several FAQ questions arrive as bare `###`), so only
figures and labels that appear verbatim in the converted pages are quoted.

| Witness | Product | URL | HTTP | gm fetch dispatch id | sha256 of converted content | timestamp (UTC) | chars |
| --- | --- | --- | --- | --- | --- | --- | --- |
| W1 | Redis | https://upstash.com/pricing/redis | 200 | 1794388016632-7147-33bf4db9c261185d | 6ffc1ac583943e90bb4a90990d141d28d8b3bbbaf6c020e201380a9c67a53717 | 2026-11-11T09:06:56.632Z | 11528 |
| W2 | Vector | https://upstash.com/pricing/vector | 200 | 1794388079581-7167-89f5ca8c5e095afd | 7d9362196e9a37fa0608bc3b5f8fa14c0fbb12f46e5e00cefe06ffe711e0f7ac | 2026-11-11T09:07:59.581Z | 5173 |
| W3 | QStash | https://upstash.com/pricing/qstash | 200 | 1794388098361-7171-4f556765bbfb9518 | 30d68eb67bf7f3974232eb2b6d81e934cf281609505f8c4d121d6ae02f7522dd | 2026-11-11T09:08:18.361Z | 5948 |
| W4 | Box | https://upstash.com/pricing/box | 200 | 1794388103765-7176-4ce72c7129e93a9b | 01b43d0729eadbd3647efd502d9adf8a023c40c693edfd530949cc0dac9cf738 | 2026-11-11T09:08:23.765Z | 4747 |
| W5 | Blob | https://upstash.com/pricing/blob | 200 | 1794388108519-7180-463d30cda60c3093 | 0b5421563453f6bfaed7c2204a5e1224277ece6b891f02679fd18887f9c33439 | 2026-11-11T09:08:28.519Z | 5046 |
| W6 | Workflow | https://upstash.com/pricing/workflow | 200 | 1794388111534-7184-8af04bfb62c03e57 | cc2dc0d27631335a68296b494f50798497dc553d5af3b1f7a6de96954d4eba2d | 2026-11-11T09:08:31.534Z | 5595 |

Each table below carries the witness id, URL, HTTP status and dispatch id of every figure it quotes;
the sha256 and timestamp are in the table above. Timestamps are the epoch milliseconds embedded in
each dispatch id, rendered as UTC.

## Redis (managed data)

W1: https://upstash.com/pricing/redis, HTTP 200, dispatch 1794388016632-7147-33bf4db9c261185d.

| Plan (as printed) | Figures and labels (as printed) | Metered dimension |
| --- | --- | --- |
| Free | $0; "Data Size" 250 MB; "Monthly Bandwidth" 10 GB; "Monthly Commands" 500K | commands, GB stored, GB bandwidth |
| Pay as You Go | "$0.2" "per 100K commands"; "Data Size" 100 GB; "Monthly Bandwidth" Unlimited; "Monthly Commands" Unlimited | commands |
| Pay as You Go | "Request price" "$0.2 per 100K"; "Storage price" "$0.25 per GB"; "Bandwidth price" "Free" | commands, GB stored, GB bandwidth |
| Pay as You Go | "Commands: $0.20 per 100K commands"; "Storage: $0.25 per GB (first 1 GB free)"; "Bandwidth: Free up to 200 GB/month, then $0.03/GB" | commands, GB stored, GB bandwidth |
| Fixed 250MB | "$10/month"; "+$5/region"; "Max data" 250 MB; "Max bandwidth" 50 GB | fixed monthly |
| Fixed 1GB | "$20/month"; "+$10/region"; "Max data" 1 GB; "Max bandwidth" 100 GB | fixed monthly |
| Fixed 5GB | "$100/month"; "+$50/region"; "Max data" 5 GB; "Max bandwidth" 500 GB | fixed monthly |
| Fixed 10GB | "$200/month"; "+$100/region"; "Max data" 10 GB; "Max bandwidth" 1 TB | fixed monthly |
| Fixed 50GB | "$400/month"; "+$200/region"; "Max data" 50 GB; "Max bandwidth" 5 TB | fixed monthly |
| Fixed 100GB | "$800/month"; "+$400/region"; "Max data" 100 GB; "Max bandwidth" 10 TB | fixed monthly |
| Fixed 500GB | "$1500/month"; "+$750/region"; "Max data" 500 GB; "Max bandwidth" 20 TB | fixed monthly |
| Enterprise | "Custom"; "Max data" 10 TB; "Max bandwidth" Unlimited | custom |
| Prod Pack | "+$200/month per database" | fixed monthly add-on |
| paid accounts | "the first 10 databases have no per-database charge, and each additional database costs $0.50 per month, up to 100 databases" | per-database monthly |

The page says "Fixed plans have no command-count billing. You pay for data size, bandwidth, and
throughput limits, not per command." It also says replications "are also counted as commands" and
gives "100K writes cost $0.4".

## Vector (managed data)

W2: https://upstash.com/pricing/vector, HTTP 200, dispatch 1794388079581-7167-89f5ca8c5e095afd.

| Plan (as printed) | Figures and labels (as printed) | Metered dimension |
| --- | --- | --- |
| Free | $0; "Daily Query/Update Limit" 10K; "1 Free DB"; "Max total data" 1 GB | daily queries/updates |
| Pay as You Go | "$0.4" "per 100K requests"; "Daily Query / Update Limit" Unlimited; "Max total data" 50 GB | requests |
| Fixed | "$60/ month"; "Daily Query / Update Limit" 1M; "Max total data" 50 GB | fixed monthly, 1M daily queries |
| Pro | "Contact Us"; "Max total data" 1 TB | custom |
| Enterprise | "Learn More"; "Scales over a billion of vectors" | custom |
| Pay as You Go | "Requests: $0.40 per 100K requests"; "Storage: $0.25 per GB"; "Bandwidth: Free up to 200 GB/month, then $0.03/GB" | requests, GB stored, GB bandwidth |
| Comparison table | "Request Price" "Free" / "$0.4 per 100K" / "None" / "None"; "Storage Price" "$0.25 per GB" on paid tiers; "Bandwidth Price" "Free" / "Free" / "Free" / "$0.03 per GB" | requests, GB stored, GB bandwidth |
| paid accounts | "You can create up to 10 indexes for free and beyond this you will be charged $0.5 per index up to 100 indexes" | per-index monthly |

## QStash

W3: https://upstash.com/pricing/qstash, HTTP 200, dispatch 1794388098361-7171-4f556765bbfb9518.

| Plan (as printed) | Figures and labels (as printed) | Metered dimension |
| --- | --- | --- |
| Free | $0; "Messages per Day" 1,000; "Max bandwidth" 50 GB; "Max message size" 1 MB | daily messages |
| Pay as You Go | "$1" "per 100K messages"; "Messages per Day" Unlimited; "Max bandwidth" 50 GB | messages |
| Fixed 1M | "$180/ month"; "Messages per Day" 1M; "Max bandwidth" 1 TB | fixed monthly, 1M daily messages |
| Fixed 10M | "$420/ month"; "Messages per Day" 10M; "Max bandwidth" 5 TB | fixed monthly, 10M daily messages |
| Enterprise | "Custom"; "100M+ messages daily"; "Unlimited bandwidth" | custom |
| Pay as You Go | "Messages: $1 per 100K messages"; "Bandwidth: Free up to 50 GB/month, then $0.05/GB"; "Max parallelism: 100" | messages, GB bandwidth |
| Pay as You Go | "Active schedules: 1,000 (Free up to 1,000. Beyond that, $0.01 per active schedule.)" | active schedules |
| Prod Pack | "+$200/month" | fixed monthly add-on |

The page says "one delivery attempt to one endpoint equals one billed message" and that a retry
counts as an additional message.

## Box (pricing reference for the shadw sandbox)

W4: https://upstash.com/pricing/box, HTTP 200, dispatch 1794388103765-7176-4ce72c7129e93a9b.

| Plan (as printed) | Figures and labels (as printed) | Metered dimension |
| --- | --- | --- |
| Free | $0; "Concurrent Boxes" 10; "CPU Hours / Month" 5 | concurrent boxes, CPU hours |
| Pay as you go | "$0.10" "per active CPU hour"; "Pay for active time only. Boxes pause when idle."; "Resources" "2 vCPU, 4 GB RAM"; "Included Storage" 10 GB | active CPU hours |
| Fixed | "$8" "per box / month"; "Always-on boxes. One monthly price, no usage charges."; "Idle Timeout" "Never" | per-box monthly |
| Pay as you go | "Storage Price" "$0.1 / GB per month"; "Monthly Price" "None" | GB-month stored |
| Small | "2 vCPU" "4 GB RAM" "10 GB"; "$0.1/active CPU hour"; "$8/month" | active CPU hours, per-box monthly |
| Medium | "4 vCPU" "8 GB RAM" "30 GB"; "$0.2/active CPU hour"; "$16/month" | active CPU hours, per-box monthly |
| Large | "8 vCPU" "16 GB RAM" "30 GB"; "$0.4/active CPU hour"; "$32/month" | active CPU hours, per-box monthly |
| Pay as You Go | "Concurrent boxes: 1,000"; "CPU hours/month: Unlimited"; "Storage: $0.10 per GB/month"; "CPU hour pricing: Small: $0.10, Medium: $0.20, Large: $0.40 per active CPU hour" | active CPU hours, GB-month stored |
| Fixed (Keep Alive) | "By size: Small: $8, Medium: $16, Large: $32 per box / month"; "CPU: Included in the fixed monthly price"; "Storage: Included"; "Idle timeout: none, boxes stay on continuously" | per-box monthly |

The FAQ says "using 100% of 2 cores for one hour costs $0.2" and "using 10% of a single core for
one hour costs $0.01"; "Storage is billed at $0.1 per GB per month"; free tier "Up to 10 concurrent
boxes" and "5 CPU hours per month".

## Blob (managed data)

W5: https://upstash.com/pricing/blob, HTTP 200, dispatch 1794388108519-7180-463d30cda60c3093.

| Plan (as printed) | Figures and labels (as printed) | Metered dimension |
| --- | --- | --- |
| Free | $0; "Storage" 1 GB; "Bandwidth (Egress)" 10 GB / month | GB stored, GB egress |
| Free | "Simple Operations" 10,000 / month; "Advanced Operations" 2,000 / month | operations |
| Pay as You Go | "$0.02" "per GB stored / month"; "Storage" "$0.02 per GB" | GB-month stored |
| Pay as You Go | "Simple Operations" "$0.30 per 1M"; "Advanced Operations" "$4.50 per 1M" | operations |
| Pay as You Go | "Bandwidth (Egress)" "Free up to 1 TB / month, then $0.02 per GB" | GB egress |
| Pay as You Go | "Uploads are always free."; "Deletes are free."; "5 TB per object." | none |

The page says "Simple operations are reads: downloading an object, reading its metadata with a
HEAD, and the bucket-level GETs. Advanced operations are everything else: uploads, copies,
renames, and listing a bucket." Storage is billed "On the monthly average of your bucket's size,
not on its peak."

## Workflow (must become part of shadw metering and tiering)

W6: https://upstash.com/pricing/workflow, HTTP 200, dispatch 1794388111534-7184-8af04bfb62c03e57.

Rendition note. A second fetch of the same URL, dispatch 1794389091380-7315-3507e3d289e7a27b
(HTTP 200), was an extractive `prompt` answer capped at `max_chars: 3200`: it returned the two
comparison-table header rows verbatim, "| Tier | Monthly price | Steps per day | Max bandwidth |
Max message size | DLQ retention | Max HTTP response duration | Max parallelism" and "| Plan |
Price | Steps per day | Max message size | Max parallelism", together with "| Monthly price |
Free | None | $180 | $420" and "| Max Steps per Day | 1,000 | Unlimited | 1M | 10M", but not the
full fixed-tier row values. Those header rows are quoted here; no value absent from a fetch is
filled in. A no-`prompt` full-page fetch at dispatch 1794389305626-7279-870719f41be3fabe
(HTTP 200, `truncated: false`) and a `raw: true` fetch at dispatch
1794389860288-7151-2b14e303f2b3f79c (HTTP 200, `body_chars: 142995`, `truncated: false`) both
contain the machine-readable "Fixed plans" tier table with its two numeric rows, and the raw HTML
shows them server-rendered inside `<tbody>`, not client-rendered. This document does not quote
those two fixed-tier rows (Max bandwidth, Max message size, DLQ retention, Max HTTP response
duration, parallelism); the fixed monthly prices and daily step counts in the table below come
from the card and comparison-table rows that both renditions print.

| Plan (as printed) | Figures and labels (as printed) | Metered dimension |
| --- | --- | --- |
| Free | "$0"; "Steps per Day" 1,000; "Monthly price" "Free"; "Message price" "Free" | daily steps |
| Pay as You Go | "$1" "per 100K steps"; "Steps per Day" Unlimited; "Message price" "$1 per 100K" | steps |
| Fixed 1M | "$180/ month"; "Monthly price" "$180"; "Max Steps per Day" 1M | fixed monthly, 1M daily steps |
| Fixed 10M | "$420/ month"; "Monthly price" "$420"; "Max Steps per Day" 10M | fixed monthly, 10M daily steps |
| Enterprise | "Learn More"; "100M+ steps daily" | custom |
| Pay as You Go | "Step price: $1 per 100K steps"; "Bandwidth: Free up to 50 GB/month. Beyond that, $0.05 per GB."; "Parallelism: 100" | steps, GB bandwidth |
| Pay as You Go | "Active schedules: 1,000 (Free up to 1,000. Beyond that, $0.01 per active schedule.)" | active schedules |
| Prod Pack | "+$200/month" | fixed monthly add-on |

The page's comparison table prints "Message price" "Free" / "$1 per 100K" / "None" / "None"
although the plan overview meters "steps". Step counting as printed: "context.run",
"context.sleep", "context.sleepUntil", "context.waitForEvent" generate "ONE billed step";
"context.call" and "context.invoke" generate "TWO billed steps"; parallel steps generate "ONE extra
billed step"; "If a step execution fails and is retried, each retry is counted as an additional
billed step." The page says "Workflow is built on top of QStash."

Requirement from the operator's brief: workflow must become part of shadw metering and tiering.
This requirement is now met in code: `crates/hive-cloud/src/billing.rs` carries
`workflow_steps_per_million_cents` on `RATE_CARD` (1000.0c) and `COST_CARD` (250.0c, a stated
public-infra-derived COGS assumption, 7500 bps gross margin), `UsageTotals.workflow_steps`
contributes to `cost_cents`, `usage_metering::UsageKind::WorkflowSteps` records each executed
step, and `PlanSpec.max_workflow_steps_per_day` sets per-plan caps of 1,000 / 100,000 /
1,000,000 / 10,000,000 / 0 (unlimited). The Upstash "$1 per 100K steps" figure above is the
PUBLIC REFERENCE that informed the unit shape; shadw's 1000.0c per million is our own rate-card
value, not an Upstash price. Charging remains off unless `HIVE_BILLING_METER_RESOURCES=1` and
every SKU clears `MIN_GROSS_MARGIN_BPS`.

## What we took from it

Our rate card is `RATE_CARD: RateCard` at `crates/hive-cloud/src/billing.rs:305`, described in
code as "the SINGLE source of truth for metered pricing (the UI fetches this; the meter loop
charges with it)". `RateCard::cost_cents` combines `UsageTotals` counters with these fields.
Only fields that exist in the code are named here.

| Published dimension | Our real field | What the mapping is and is not |
| --- | --- | --- |
| Box "per active CPU hour", "CPU Hours / Month" | `active_cpu_hr_cents` ("Active (non-idle) CPU, cents per CPU-hour") | Same unit shape: active CPU time. The rate card has no per-box or keep-alive fixed-price field. |
| Redis "per 100K commands", Vector "per 100K requests", QStash "per 100K messages" | `requests_per_million_cents` ("Edge requests, cents per million") | Our general request meter. `RateCard` also has `waf_per_million_cents`, `browser_per_million_cents`, `db_row_ops_per_million_cents` and `workflow_steps_per_million_cents`; there is no separate command or message SKU, but workflow steps now have their own. |
| Workflow "per 100K steps" | `workflow_steps_per_million_cents` on `RATE_CARD` / `COST_CARD`; `UsageTotals.workflow_steps`; `UsageKind::WorkflowSteps`; `PlanSpec.max_workflow_steps_per_day` | A dedicated step SKU now exists with its own price, COGS, usage kind and per-plan cap. The Upstash price is a public reference; our 1000.0c / 250.0c values are our own. |
| Blob "Simple Operations" "$0.30 per 1M", "Advanced Operations" "$4.50 per 1M" | `requests_per_million_cents` (general requests); `db_row_ops_per_million_cents` ("Managed-database row operations, cents per million reads/writes") | Blob operation counts are request-shaped; the managed-data row meter is a distinct field for row reads/writes, not object operations. |
| Redis and Vector "Storage price" "$0.25 per GB" | `db_storage_gb_month_cents` ("Managed-database storage, cents per decimal GB-month") | Both are stored GB; our field is a decimal GB-month, computed from `db_storage_byte_ms` via `BYTE_MS_PER_GB_MONTH`. |
| Box "Storage Price" "$0.1 / GB per month"; Blob "$0.02 per GB stored / month" | `object_storage_gb_month_cents` ("Object storage, cents per decimal GB-month") | Our object tier is a decimal GB-month, computed from `object_storage_byte_ms`. |
| Redis, Vector, QStash, Workflow "Bandwidth"; Blob "Bandwidth (Egress)" | `ingress_gb_cents` ("Inbound network transfer, cents per decimal GB") and `egress_gb_cents` ("Outbound network transfer, cents per decimal GB") | Our card splits direction; the Upstash pages print one "Bandwidth" line, and Blob alone names "Egress". `UsageTotals` carries `ingress_bytes` and `egress_bytes`. |
| Free allowances: "Data Size" 250 MB, "Storage" 1 GB, "CPU Hours / Month" 5, "Concurrent Boxes" 10, "Messages per Day" 1,000, "Steps per Day" 1,000, "Daily Query/Update Limit" 10K | `PlanSpec` quotas: `max_projects`, `max_members`, `max_sandboxes`, `max_running_sandboxes`, `max_sandbox_mounts`, `max_sandbox_env_vars`, `max_sandbox_ports`, `max_drive_bytes`, `max_workflow_steps_per_day`, plus `included_cents`, `overage` and `overage_ceiling_cents` | Code says "The `max_*` fields are the enforced quotas (0 = unlimited)". `max_workflow_steps_per_day` is the per-day step quota (1,000 / 100,000 / 1,000,000 / 10,000,000 / 0); there is no separate per-day message or command quota. |

The 5 plans in `PLANS` are `hobby`, `pro`, `business`, `enterprise` and `enterprise_plus`; each
`PlanSpec` also has `id`, `name`, `price_cents`, `stripe_price_id` and `features`. Upstash's
"Free", "Pay as You Go" and "Fixed" tier labels are not `PlanSpec.id` values; `enterprise` and `enterprise_plus` are our own ids with their own `price_cents`.

## Charging gates

`RATE_CARD.assert_profitable()` requires every priced SKU to clear `MIN_GROSS_MARGIN_BPS`, which
is `6_000` basis points, against `COST_CARD`; `margin_report` computes
"(price - cogs) / price * 10_000" and `assert_profitable` returns the names of SKUs below the
floor. `resource_charging_enabled()` returns true only when `HIVE_BILLING_METER_RESOURCES` is
set to `1` (the code also accepts `true`) AND `RATE_CARD.assert_profitable().is_ok()`; its
comment calls this a "Financial ONE-WAY DOOR" with "Default is FALSE (shadow mode: meters measure,
nobody is charged)". Charging therefore stays off unless `HIVE_BILLING_METER_RESOURCES=1` and the
margin check passes. No Upstash figure in this document is part of that check.
