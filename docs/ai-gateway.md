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

`GET /v1/ai/state` and `/v1/admin/ai/economics` are platform-specific. The five
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
economics at `GET /v1/admin/ai/economics` is Admin plus `require_operator`. All
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
published list price. `latency` (time-to-first-token), all `native_tokens_*`
fields and `billable_web_search_calls` are `null` because a buffered relay cannot
measure provider-native tokenization, TTFT or a feature that is not implemented.
`generation_time` is the real wall clock. A not-yet-ingested id answers 404 with
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
response, the body stream aborts and the socket closes — not just a counter
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
  and tokens. Web search is not implemented and has no meter, rather than a
  fabricated zero.
- Vercel's three inference APIs require `model` and `messages`/`input` and
  support `providerOptions.gateway`, reporting headers and tool/structured-output
  fields. This gateway validates only `model` and translates plain text messages;
  tools, structured output, reasoning options and reporting tags are not
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

The first nine, the contract replay (`contract-billing`: probes=11,
replay_ok=true, signed=11, proofs=9) and witness 10 are recorded with their run
identities in `.gm/witness-log.md`. Witness 11 is recorded when it completes.
Local commits only: no push and no roll without the operator word.
