# AI Gateway operator reference

Present-tense operator reference for the AI Gateway REST surface, its provider pool,
pricing, billing, held items and residuals. Local work only: no push and no roll
without the operator word.

## What it is

The platform mounts a Vercel-AI-Gateway-shaped REST surface at these exact
documented paths:

| Method and path | Purpose |
| --- | --- |
| `GET /v1/models` | OpenAI models-list shape; `?include_availability=true` |
| `GET /v1/models/:model/endpoints` | provider endpoint list; also `:creator/:model` |
| `POST /v1/chat/completions` | OpenAI Chat Completions shape |
| `POST /v1/responses` | OpenAI Responses API shape |
| `POST /v1/messages` | Anthropic Messages shape |
| `GET /v1/credits` | `balance` as a USD string, `total_used: null`, `token_pass: null`, `currency`, `residuals` |
| `GET /v1/generation?id=gen_<id>` | one generation record in a `data` envelope, or 404 |
| `GET /v1/report` | bounded, node-labelled generation log over an inclusive UTC date range |
| `GET /v1/admin/ai/economics` | operator-only COGS and margins |
| `GET /v1/ai/state` | dashboard aggregate, platform-specific, node-labelled |
| `POST /v1/ai/web-search` | platform-specific standalone bounded web search |

`GET /v1/ai/state`, `POST /v1/ai/web-search` and `/v1/admin/ai/economics` are platform-specific. The five
Vercel REST reference paths are `/v1/models`, `/v1/models/{creator}/{model}/endpoints`,
`/v1/credits`, `/v1/generation`, `/v1/report`; the three inference POSTs come from
Vercel's separate SDK/API pages (Chat Completions, Responses/OpenResponses,
Anthropic Messages) and are mounted here with OpenAI/Anthropic-shaped bodies.
Every error body is one `{"error":{"message","type"}}` object. Types used are
`invalid_request_error`, `authentication_error`, `permission_error`,
`not_found_error`, `insufficient_quota` and `api_error`; emitted statuses are
400, 401, 402, 403, 404, 502 and 503. `429` and `500` are NOT emitted: there is
no per-team availability rate limit and no internal-error classifier, both
listed as residuals. Vercel documents a `type` discriminator on *some* endpoints;
this platform emits it on all, which is a documented shape difference.

This is NOT a byte-for-byte clone of the Vercel 518-model catalog. There is no
BYOK, no OIDC, no budgets, no embeddings, images or audio, no regional hosting
and no trace drains. The catalog holds 16 rows with true provenance, not 518.
Real `released` dates, `tags` and long-context price tiers come from the same
2026-11-18 live catalog fetch as the base prices. Twelve catalog ids are bare
tokenhun ids rather than Vercel's `creator/model-name` form, which is why the
endpoints route accepts a single-segment `:model` alias.

## Auth and roles

Every mounted route EXCEPT the two public catalog reads requires a platform JWT
or a tenant API key in `Authorization: Bearer`. `GET /v1/models` and
`GET /v1/models/{creator}/{model}/endpoints` are unauthenticated, exactly as
Vercel documents, and carry ONLY static catalog rows (no tenant, no URL, no
node name). Tenancy on every other route comes from the verified claim, never
the `x-hive-team` header. POST inference on `POST /v1/chat/completions`,
`POST /v1/responses` and `POST /v1/messages` is a Member-floor write. Operator
economics at `GET /v1/admin/ai/economics` is Admin plus `require_operator`. `POST /v1/ai/web-search` is a Member-floor write, listed deliberately in `roles::RULES` because an unlisted write is ADMIN by default. All
tenant-bearing routes fail closed and are NOT added to `is_open_route`; a role
is decided once by `roles::RULES` through `roles::gate`, and a tenant read never
fails open on missing auth.

## Live Vercel REST contract (fetched 2026-11-18)

The live Vercel REST doc answers `GET /v1/models` and
`GET /v1/models/{creator}/{model}/endpoints` WITHOUT authentication, while
`GET /v1/models?include_availability` REQUIRES authentication (401 without it)
and adds `model_eligibility` plus request-level availability fields. This
platform matches all three behaviours. Annotated responses carry
`Cache-Control: private, no-store`; because team blocking, verification, funds,
quota and live provider health are not evaluated here, they report
`request_context_availability.http: "unknown"` and `availability_status:
"partial"`, never a fabricated "complete"/"eligible". A `model_eligibility`
status of `eligible` additionally requires a RESOLVED provider credential.
Vercel's per-team 429 + `Retry-After: 60` on annotated requests is not
implemented.

`GET /v1/credits` returns `balance` as a USD STRING taken from the real prepaid
billing ledger, including a negative balance (never clamped to zero).
`token_pass` is always `null` (no Token Pass exists). `total_used` is `null`,
because the platform ledger tracks all-resource spend, not AI-only credits;
labelling that number as AI credits used would be false. The body carries a
`residuals` array stating all three facts. `currency` is `"USD"`.

`GET /v1/generation?id=gen_<id>` returns one `data` envelope. Generation ids are
`gen_<32 hex>` (UUID v4 simple), NOT Vercel's `gen_<ULID>`; that is a documented
residual. Nothing is debited from an AI Gateway wallet on this platform, so
`total_cost`, `gateway_cost`, `usage`, `surcharge_cost` and
`upstream_inference_cost` are `null`. The measured market price is exposed under
its own honest name `public_price_usd`, and `market_cost` carries the same
published list price. `latency` (time-to-first-token) and all `native_tokens_*`
fields are `null` because a buffered relay cannot measure provider-native
tokenization or TTFT. `billable_web_search_calls` is the MEASURED integer when
a web-search tool was requested and `null` when no web-search tool was asked
for; `web_search_status` names `billed`, `empty_results`, `failed`,
`unconfigured` or `not_requested`. `generation_time` is the real wall clock. A not-yet-ingested id answers 404 with
the fixed sentence 'Usage event not found'.

`GET /v1/report` REQUIRES `start_date` and `end_date` in YYYY-MM-DD UTC,
inclusive, with an optional `group_by`. This is a bounded list of at most 512
individual generation rows from THIS node, NOT Vercel's aggregated spend API:
Vercel returns `results[]` with eight `group_by` values, `date_part`, ten
filters, tag/userId/quota-entity attribution, a per-query charge, a 403 plan gate
and a 402 unpaid-query refusal. None of those are implemented. The response
names `node_local: true` and `partial: true` and carries no aggregation.

## Provider pool

`HIVE_AI_GATEWAY_PROVIDERS` is an operator JSON array. Each entry carries `name`,
`base_url` or `base_url_env`, `protocol` (`openai-chat` or `anthropic-messages`),
`credential_env`, `credential_header`, `models`, `priority` and `timeout_ms`.
`credential_env` is the NAME of an environment variable that holds the
credential; the value resolves SERVER-SIDE on the node that serves the request.
A credential value never enters config, `state.json`, logs, responses or this
repo. A provider with no resolved credential is reported `unavailable`, never
fatal.

Three defaults exist, listed by provider name and env var NAMES only:

| provider | credential env var name | other env var name |
| --- | --- | --- |
| `local-relay` | `HIVE_AI_GATEWAY_RELAY_TOKEN` | none |
| `tokenhun` | `HIVE_AI_GATEWAY_TOKENHUN_TOKEN` | none |
| `opencode` | `HIVE_AI_GATEWAY_OPENCODE_TOKEN` | `HIVE_AI_GATEWAY_OPENCODE_BASE_URL` |

Two defaults use the OpenAI Chat Completions protocol and one uses Anthropic
Messages. The loopback relay `local-relay` listens at 127.0.0.1:8792 on ONE Mac.
It is not a fleet-wide service: fleet nodes see it unavailable and fail over to
the next provider. Provider URLs are operator config, never tenant-influenced:
each is parsed with `reqwest::Url`, userinfo, query and fragment are refused,
and the path is built with `Url::set_path` from a fixed constant. Calls use
`hive_outbound::internal_client()` with no redirects and no proxy. Never set
`HIVE_OUTBOUND_ALLOW_PRIVATE` on an enforced node: that developer escape is
refused when `HIVE_JWT_SECRET` is set. No credential, Authorization header or
full upstream body is logged.

## Failover and bounds

`HIVE_AI_GATEWAY_MAX_ATTEMPTS` defaults to 3 with a hard cap of 4. ONE
absolute wall deadline `HIVE_AI_GATEWAY_TOTAL_DEADLINE_MS` (default 90000 ms,
hard cap 110000 ms) covers every attempt AND the upstream body read; it is
strictly under `ADMIN_REQUEST_TIMEOUT_SECONDS_DEFAULT = 120`, so the admin
router's `TimeoutLayer` cannot truncate a chain mid-flight. Each attempt takes
`min(per_provider_timeout, remaining_deadline)`. The upstream response is read
by `read_bounded_owned`, not `hive_outbound::read_limited`: the latter has a
byte cap but NO wall deadline, so a slow-drip provider could hang forever.
`read_bounded_owned` bounds bytes (2 MiB non-stream, 4 MiB SSE), checks the
deadline inside EVERY `chunk().await`, and does NO line parsing, so a
newline-free SSE line cannot grow an unbounded String. `stream=true` is a
BUFFERED relay of the provider's own frames, not an incremental stream.
Retry happens on a connect error, a timeout, or an upstream 401/403/404/408/
429/5xx: an upstream auth failure means OUR configured credential is wrong,
not that the caller is unauthenticated, so it fails over and NEVER propagates
as a caller 401. A 400 validation error returns immediately. There is NO
retry after the first response byte, so a client is never billed twice; the
SSE_FIRST_BYTE_NO_RETRY live witness measures provider A posts=1, fallback
posts=0 and one streamed generation. All tenant-facing failures use FIXED
safe sentences; the upstream URL, status and body are never forwarded.
`HIVE_AI_GATEWAY_MAX_INFLIGHT` (default 32) bounds concurrency and answers 503
`api_error` when full. An `Inflight` guard OWNS the `reqwest::Response`: on
client disconnect, axum drops the handler future, the guard drops the
response, the body stream aborts and the socket closes, not just a counter
decrement. `copy_bidirectional` and a `select!` over a splice are never used.
`internal_client()` now sets `.no_proxy()`; a live witness with a dead hostile
`HTTP_PROXY` confirms the loopback provider still receives the request.

## Injected content and provenance

The ONLY injected system rule is the language rule.
`HIVE_AI_GATEWAY_SYSTEM_PROMPT` defaults to text that follows the requested
language and does not reply in Chinese unless the user explicitly asks for
Chinese. It is added as an extra system message (Chat Completions), the
`instructions` field (Responses API) or the `system` field (Anthropic
Messages), before the caller system content, bounded to 4096 bytes and never
logged. NO identity-impersonation prompt is injected: the
platform never claims the model is Anthropic Claude and never presents an
upstream model to a paying customer under another vendor brand. `GET /v1/models`
discloses `owned_by` (the real provider), `upstream_model` (the real upstream
id), `provider` and `provenance`. The two operator aliases served by the local
relay carry an explicit provenance string stating that the platform has NOT
verified an Anthropic upstream. A `discontinued`, `pre-offline` or `held` model
returns 400 `invalid_request_error` with the fixed sentence "this model is not
available on this gateway" and is never silently routed. On the plain public
`/v1/models` (which Vercel defines as "list all available models") these rows
also appear with `status` and `available: false`, so a programmatic client can
tell them apart. `model_not_available` is not a Vercel error type.

## Relay model aliases

The two relay request aliases are now Sonnet 5.5 / Haiku 5.5. Canonical ids stay
NEUTRAL (`shadw/relay-sonnet-5-5` and `shadw/relay-haiku-5-5`), `owned_by` stays
the real provider (`shadw-relay`), `provider` stays `local-relay`, and
`upstream_model` is `hy4` / `hy4-flash`. `GET /v1/models` `provenance` states
that the platform has NOT verified an Anthropic upstream. NO identity-impersonation
prompt is injected; the ONLY injected system rule is the language rule. The old
ids and old aliases remain as LEGACY aliases so existing callers keep resolving.
The relay catalog rows carry the real Sonnet 5.5 / Haiku 5.5 released dates, tags
and cache prices from the live catalog read; that is a price disclosure, not a
claim of an Anthropic upstream.

## Pricing and billing

Public prices are pinned to the published USD/token prices in the Vercel model
catalog read from `GET https://ai-gateway.vercel.sh/v1/models` fetched
2026-11-18 (518 rows, HTTP 200). `AI_MODELS` is the ONE canonical per-model
price card. COGS is a STATED operator assumption:
`HIVE_AI_GATEWAY_COGS_BPS` (default UNSET, no invented default participates
in a real-money decision) is a fraction of the public price, parsed into
0..=3900. When unset, `ai_margin_rows()` returns ZERO priced rows. Long-context
price tiers from the same live catalog ARE honoured in the charge calculation,
so a 300K-token request is not undercharged 5x against the published price.
Regional, service-tier, `fast` and `varies_by_provider` pricing are not.

The Vercel public pricing page states, verbatim:

  > AI Gateway charges no markup and no platform fee on tokens; you pay the provider's list price

That CONFLICTS with the operator request to sell at a markup so the lane is
profitable. The resolution here is a Vercel-parity PUBLIC price, a documented
operator COGS assumption and opt-in charging. No claim is made that the platform
undercuts, matches or reproduces the Vercel margin model; the profitability
check is an internal gate against the assumed COGS, not a measurement of any
provider invoice.

`ai_charging_enabled()` requires ALL of `resource_charging_enabled()` (which
itself needs `HIVE_BILLING_METER_RESOURCES=1`), `HIVE_AI_GATEWAY_CHARGING=1`, a
COGS bps that parses into 0..=3900, `RATE_CARD.assert_profitable().is_ok()` AND
a SEPARATE `billing::ai_profitable()`. Charging is OFF by default and no money
moves until the operator opts in with a measured COGS. The AI margin check
CANNOT FAIL for any admissible COGS bps: cogs is defined as a fraction of price
in 0..=3900, so gross margin is >= 6100 bps by construction. It is an internal
gate against a stated assumption, NOT a measured profitability proof. The
cap is 3900 rather than 4000 because `((P - 0.4P)/P * 10000) as i64` truncates
to 5999 for the pinned price 0.00000396, which would fail the 6000 floor.
Every cents amount is bounded by `billing::MAX_AMOUNT_CENTS` with saturating
arithmetic; `cost_cents` clamps the AI term before dividing so a saturated u64
can never make `meter_usage` refuse the WHOLE sum after advancing the cursor.

There is ONE `may_act(BillingMeter)` writer: the six AI fields fold into the
SAME `resource_charging_enabled()` block of `spawn_billing_meter_loop`, never a
second loop. The six additive `serde(default)` named meters are
`ai_public_micro_cents`, `ai_cogs_micro_cents`, `ai_input_tokens`,
`ai_output_tokens`, `ai_requests` and `ai_web_search_calls`, added to BOTH
`billing::UsageTotals` and `usage_metering::TenantUsage`. They never widen
`COUNTER_KEYS` or any existing `[u64; 19]` or `[f64; 19]` catalog array.
`cost_cents` converts a CLAMPED `ai_public_micro_cents` by dividing by
1,000,000 (a micro-cent is a fixed-point unit, not a hidden price).
`RateCard::margin_report()` is deliberately LEFT UNTOUCHED: appending AI rows
there would let a bad AI env var turn off ALL non-AI resource charging fleet-wide.
A separate `billing::ai_profitable()` checks `ai_margin_rows()` instead.
`meter_usage` additionally zeroes `delta.ai_public_micro_cents` when AI charging
is off (defence in depth). The signed oplog gains a THIRD additive signed op `meter.ai.sample` with
its own 8 declared payload keys; `CONTRACT_VERSION` stays 1 and the legacy
`meter.sample` and `meter.catalog.sample` payloads stay byte-identical, so
every existing signed entry still replays. `contract-billing` submits one real
sample and proves its inclusion.

## Web search

Web search is a SERVER-SIDE tool the platform executes, not a provider-native
feature. Vercel has NO standalone `POST /v1/web-search` REST endpoint; the
platform-specific `POST /v1/ai/web-search` is a convenience route for one
bounded search. Body `{"query":"...","max_results":N}` answers
`{"object":"list","data":[{"title","url","snippet"}],"billable":bool,
"price_per_1000_usd":"10.00"}`. Tenant comes from the verified claim, never a
header. `billable=false` carries a printed reason; failure is never reported as
a successful search. All tenant-facing errors are FIXED safe sentences; no
upstream URL, status or body is echoed.

An explicit allowlist is detected in an inference request `tools` array:
`web_search` (Responses/OpenAI), `web_search_20250305`, `web_search_20260209`
and `web_search_20260318` (Anthropic Messages), plus the six Vercel built-ins
`vercel:{perplexity,parallel,exa,browserbase,deepseek,tako}_search`. Every
built-in runs through the SAME bounded platform provider; none selects a
separate provider or price. A `vercel:*_fetch` or `web_fetch*` tool is refused
with a fixed error, because its `config.url` is tenant-controlled. ONE bounded
search runs BEFORE the provider attempt loop, inside the ONE absolute inference
deadline. The query comes from an EXPLICIT `config.query` / `config.objective`
when present, otherwise the LAST user message text; both are bounded and only
those two config keys are read.

Results are injected as a plain-text, bounded, explicitly UNTRUSTED `user`
message, NEVER into `system` or `instructions`: untrusted page text must not
outrank the operator's language rule, and cross-protocol translation flattens
only plain text. ALL client-defined `tools` and `tool_choice` are removed from
the upstream payload, so a native upstream server tool cannot bill the operator
a second time. A failed or unconfigured search returns a fixed Vercel-shaped
error with a distinct `web_search_status` rather than silently degrading to
ungrounded inference.

Operator config holds ENV VAR NAMES only: `HIVE_AI_WEB_SEARCH_URL` (trusted
base), `HIVE_AI_WEB_SEARCH_CREDENTIAL_ENV` (NAME of the env var holding the
credential, default `HIVE_AI_WEB_SEARCH_TOKEN`), and
`HIVE_AI_WEB_SEARCH_TIMEOUT_MS`. NO credential value or key literal enters
config, `state.json`, logs, responses or this repo; the value resolves
SERVER-SIDE on the node that serves the request. The base is parsed with
`Url::parse`, userinfo, query and fragment are refused, the path is set with
`Url::set_path` from the FIXED constant `/v1/web-search`. A public destination
uses `hive_outbound::client()` (public-only resolver, no redirect, no proxy);
an operator-configured loopback or self-hosted provider uses
`hive_outbound::internal_client()` (no redirect, no proxy), the same sanctioned
surface as `HIVE_AI_GATEWAY_PROVIDERS`. The URL is validated at registration
AND before every request. NO tenant string ever reaches a URL: the query goes
in the POST JSON BODY. Every result URL is checked with
`hive_outbound::validate_fetch_url` before it is returned as text, so
`javascript:`, userinfo and non-public hosts are refused; result URLs are never
fetched and never interpolated into HTML or `Location`. Result count, query,
title, snippet, context and body bytes are bounded and truncated.

Tool-driven inference prices a search from THIS model's own published
`pricing.web_search` (USD per 1,000 searches). A model with no published
`web_search` price REFUSES the tool with a fixed error instead of inventing a
price. The four catalog rows documenting `web_search: "10"` therefore bill
1 cent = 1,000,000 micro-cents per call. The platform-specific standalone route
has no model context and uses that same published "10.00" tier. Vercel also
publishes 1, 2.5, 5, 14 and 35 USD tiers and separate provider-specific
built-in prices; those per-variant prices are NOT implemented.

A billable call is a SUCCESSFUL search returning a NONEMPTY results array;
failed requests, timeouts, empty results and an unconfigured provider are NOT
billed. One billable call writes ONE measurement into the node-local metering
sidecar: `UsageKind::AiWebSearchCalls` (+1), `UsageKind::AiPublicMicroCents`
(+price) and `UsageKind::AiCogsMicroCents` (+price times `cogs_bps()/10000`,
zero when COGS is unset). The ONE `may_act(BillingMeter)` writer folds
`ai_public_micro_cents`; there is no second fold loop. NO new `UsageTotals`
fields and NO `[u64; 19]` widening: `billing::RateCard::cost_cents` already
sums `ai_public_micro_cents`, and `METER_AI_SAMPLE` already declares
`ai_web_search_calls`. `ai_margin_rows()` gains `ai_web_search_call` with the
SAME stated-assumption fraction, so `billing::ai_profitable()` covers it without
being able to switch all AI charging off. Charging stays behind
`ai_charging_enabled()`, which is OFF by default: with charging off the operator
absorbs the upstream search cost (shadow mode). The first meter tick zeroes the
AI baseline, so a search measured before that tick is a measurement, not a
charge. The generation row reports the MEASURED `billable_web_search_calls` and
`web_search_status`; `state_value` discloses `web_search.configured`,
`price_per_1000_usd`, `max_results`, measured calls and
`rate_card.web_search_per_call_usd`.

Printed web search residuals: ONE search per request, no model-driven multi-step
tool loop, no `pause_turn` continuation, no provider-native `server_tool_use`
blocks, no citations or annotations, no dynamic filtering, no WEB FETCH tool,
no free Parallel `fast` mode, no provider-specific built-in search variants
(all built-ins share the one platform provider), client-defined tools are not
forwarded, result URLs are validated but never fetched, and charging OFF by
default means the operator absorbs the upstream search cost. Tag/user/quota
reporting attribution, the charged `/v1/report` query, the Hobby/Pro-trial 403
plan gate and the per-team 429 rate limit remain unimplemented and are
unchanged by this feature.

## Held and refused

ChatGPT.com consumer-web automation and residential-proxy scraping are NOT
implemented. `openai/gpt-6-sol` is catalogued as HELD with a printed reason:
consumer-web automation is refused, and an operator may add a legitimate API
provider for it through `HIVE_AI_GATEWAY_PROVIDERS`. The two keys pasted into
the request are NOT stored, printed or committed anywhere; they appeared in a
chat transcript, so the operator rotates them.

## Residuals

- No BYOK, no OIDC, no budgets, no embeddings, images or audio, no regional
  hosting, no trace drains and no 518-model catalog.
- The two public catalog reads are now unauthenticated like Vercel's; every
  OTHER mounted route requires a verified claim and none is an `is_open_route`
  exemption.
- `token_pass` is always `null` here; `GET /v1/credits` maps to the platform
  prepaid BILLING balance (including a negative balance), not a separate Vercel
  AI credit wallet, and `total_used` is `null` because the ledger tracks all
  resources rather than AI-only credits.
- Generation cost fields (`total_cost`, `gateway_cost`, `usage`,
  `upstream_inference_cost`, `surcharge_cost`), `latency` and all
  `native_tokens_*` are `null`, because nothing is debited from an AI wallet and
  a buffered relay cannot measure TTFT or provider-native tokenization. The
  published market price is reported separately as `public_price_usd`.
- Generation ids are `gen_<32 hex UUID>`, not Vercel's `gen_<ULID>`.
- `/v1/report` is a bounded node-local generation log, NOT Vercel's charged,
  tag/user/quota-aggregated spend API: no `results[]`, no eight `group_by`
  values, no `date_part`, no ten filters, no per-query charge, no 403 plan gate.
- `429` and `500` are not emitted; the per-team availability rate limit, the
  `Retry-After: 60` header and an internal-error classifier are absent.
- Provider reachability is not probed on a request path, so `pool[].available`
  is `null` rather than a config check relabelled as availability. The dashboard
  reads prices only from the `rate_card` wire field.
- `internal_client()` now sets `.no_proxy()` so ambient `HTTP_PROXY`/`ALL_PROXY`
  cannot route an operator-configured loopback provider bearer to a third party.
  This fixes the shared primitive for ALL fleet-internal callers, not just AI.
- The mesh `GOSSIP_GET` arms inherit the pre-existing `mesh_tenant()` rolling-
  upgrade fallback: under enforcement, a peer hop with NO signed `tok` param
  falls back to a raw `?team=` value with only a WARN. This is NOT introduced by
  the AI gateway, is bounded to holders of a mesh trunk, and is gated on the
  real fleet by `HIVE_PEER_TRUST=1` + `HIVE_TRUSTED_NODE_IDS`. HTTP AI routes do
  NOT use this path: they read only the verified `Claims.tenant`. The comment on
  the AI arms referring to `mesh_tenant()` as "never a raw team" is inaccurate;
  the accurate statement is above.
- The generation record ring is node-local and bounded by
  `HIVE_AI_GATEWAY_MAX_GENERATIONS` (default 512); it is NOT replicated.
- `GET /v1/generation` and `GET /v1/report` fan out to healthy peers with
  `?local=true` under ONE 3 s budget; a response names `node` and is labelled
  `partial` when a peer misses.
- `stream=true` is a BUFFERED, deadline-and-byte-bounded SSE relay (not
  incremental); cross-protocol streaming returns a clear 400 and streaming token
  usage is not measured, so the streaming generation row carries `null` costs
  and tokens.
- Web search IS implemented as a server-side tool and a platform-specific
  standalone `POST /v1/ai/web-search` route: ONE bounded search per request,
  an operator-configured provider named by ENV VAR NAMES only, priced from each
  model's published `pricing.web_search` (the standalone route uses the 10.00
  USD per 1,000 tier), billable ONLY on a successful nonempty result, no charge
  on failure, empty results or an unconfigured provider, gated by the existing
  `ai_charging_enabled()`, and the MEASURED `billable_web_search_calls`. The
  printed limitations are in the "Web search" section above.
- Vercel's three inference APIs require `model` and `messages`/`input` and
  support `providerOptions.gateway`, reporting headers and tool/structured-output
  fields. This gateway validates only `model` and translates plain text messages;
  ONLY the web-search tool shapes are executed server-side (see "Web search");
  other tools, structured output, reasoning options and reporting tags are not
  implemented.
- When a provider omits `usage`, streamed token accounting is unavailable; no
  tokens are invented.
- COGS remains a stated assumption until the operator supplies a measured
  figure; charging stays OFF.

## Verify

The witnesses recorded in `.gm/witness-log.md` and
`docs/PENDING_VALIDATION.md` are, each with its own run identity:

1. `cargo test --workspace --locked`
2. `scripts/cli-acceptance.sh`
3. `python3 -I scripts/check-contract-bindings.py --check`
4. `git diff --check`
5. `npx tsc --noEmit`
6. `npx tsc --noEmit -p tsconfig.ci.json`
7. `npm run lint`
8. NO_TEST_FILES
9. secrets_gate
10. Live execution of the REAL `target/debug/hive-cloud` binary against a fresh
    empty `HIVE_DATA` (no production state), five loopback stubs and the real
    127.0.0.1:8792 relay proving 401 then failover, plus an SSE first-byte
    no-retry witness (one upstream POST, zero fallback POSTs).
11. Headful Chromium at 320/360/390/768/1024/1440 on a production build of a
    scratch copy served at `http://localhost:PORT` (never 127.0.0.1).

The eleven witnesses are recorded with their run identities in
`.gm/witness-log.md`. The headful six-width witness passed via an independent
raw-CDP driver after the `bsk` CLI refused every DOM action with
`cooperation_sync_required`; that first attempt is recorded as NOT WITNESSED,
not as a pass. Measured at 320/360/390/768/1024/1440: `overflow=0`,
`clippedCount=0`, zero console errors, `h1="AI Gateway"`, provider pool,
residuals and `rate_card` prices present. The three sub-44px buttons reported
on the page come from the pre-existing global `ui/components/gitops.tsx` (the
new view has no `<button>` at all) and `chromeSmall=5` is identical on `/usage`.
Local commits only: no push and no roll without the operator word.

Web search residuals are listed in the "Web search" section and in Residuals
above. The witnesses for web search are PENDING, not run: a real
`target/debug/hive-cloud` against a fresh empty `HIVE_DATA` plus a loopback
search stub (tool-driven injection, standalone bounded results, one billable
call recording exactly one `ai_web_search_calls` and +1,000,000 micro-cents,
empty results with `billable=false` and zero charge, an unconfigured provider
with a fixed 400, no secret leak, charging OFF by default and relay 5.5 alias
resolution), a headful six-width production-build browser witness, and the
three rows in `docs/PENDING_VALIDATION.md`. No web search witness has run here.
