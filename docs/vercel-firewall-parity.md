# Vercel Firewall parity: implemented scope and residuals

Status: IMPLEMENTED (local). Attack Challenge Mode, always-on sensitive-path
denial, bot management, firewall observability and page-integrity DETECTION
are live on the edge request path. This is NOT a 1:1 Vercel Firewall clone.

## Attack Challenge Mode

`PUT /v1/projects/:project/firewall/attack-mode` with
`{"enabled":true,"hours":1|6|24}`. The setting is an additive
`#[serde(default)]` `AttackMode` field on the replicated `ProjectSettings`
row, written through `touch`, so the existing per-row `MERGE_STORES` merge
carries it to every node. Auto-off is LAZY against `active_until_ms`; there
is no timer and no second background loop. The handler asserts
`has_explicit_caller_identity` BEFORE resolving any tenant, then binds the
row with `require_project_claiming_row`. Roles floor is Admin.

While active, all traffic (including legitimate users, as Vercel documents)
receives a bounded JavaScript-capability challenge; non-idempotent methods
(POST/PUT/DELETE/PATCH) are refused with a fixed 503 so the JavaScript solve
never silently loses a request body. EventSource/SSE clients cannot execute
the challenge and are covered by the same refusal. The page has NO form
action, NO caller-controlled HTML and NO Location. A sealed short-lived
nonce in the page is re-fetched on the SAME path via the
`x-hive-challenge` header; solving it sets a server-sealed `hive_fw` cookie
(`v1.<expiry_ms>.<HMAC>`, HttpOnly, SameSite=Lax, Secure unless plain-http
loopback). Cookies are bound to (project, host, mode generation, expiry) with
a distinct HMAC domain separator over the fleet-shared at-rest key, verified
with `admin::ct_eq`. A `verified_bot_bypass` opt-in is OFF by default.

## Sensitive-path denial (always-on system mitigation)

The gate runs at the TOP of `edge_pipeline_inner`, BEFORE ACME, before any
canonical/region/rename redirect and before the rate limiter. Unambiguous
secret, VCS and framework-debug paths are refused with a fixed 403, so a
credential-harvest probe can never be answered with a 307 or a 404. This is
INDEPENDENT of Attack Mode. AMBIGUOUS names (config.js, settings.json,
app.js, Dockerfile) are never hard-denied; they are only challenged while
Attack Mode is active, so a real tenant asset is not broken. Probe names
such as `actuator` match only the last path segment, so `/actuator/health`
stays a valid tenant route.

## DDoS mitigation (L7 only)

L7 rate limiting reuses the existing shared `RateLimiter` primitive and the
existing 429 path; no new limiter and no new loop. Vercel's L3/L4 volumetric
absorption, anycast scrubbing and automatic network-layer mitigation are NOT
implemented and cannot be implemented in application code. Verified Vercel
documented values (WAF Rate Limiting default 60 s / 100 requests, per-region
counters, Fixed Window on all plans) are not replicated here.

## Bot management and the verified bots directory

A bounded, operator-reviewed allowlist of seven verified-bot names with
their UA substring and `verification_method: "ua-substring (spoofable)"`.
Vercel's public directory is `https://bots.fyi` (247 entries, human-facing);
no machine-readable verified-bots JSON/API is documented and none is
fabricated here. `bot_status` exposes Vercel's documented vocabulary
(`verified`, `identified`, `impersonation`, `unknown`) as a UA-derived
MEASUREMENT. UA matching alone is SPOOFABLE: there is no reverse-DNS or IP
verification, no Web Bot Auth, no bot score, no JA4/JA3 and no BotID private
signal network. Vercel's own docs state verification uses IP ranges,
forward-confirmed reverse DNS and cryptographic Web Bot Auth; none of those
is claimed here.

## Firewall observability

A bounded node-local ring of 512 decisions (ts, project, bounded path,
status, action, bounded reason), plus the existing `CloudState::record` event
path. Actions follow Vercel's documented vocabulary: allow, bypass,
challenge, challenge-solved, challenge-failed, deny, log, rate_limit,
redirect. `GET /v1/firewall/state` and `GET /v1/firewall/events` report
`node_local: true` and NEVER print a fabricated fleet total. No IPs, cookies
or tokens are stored in the ring. Vercel's log retention, drill-down, Query
builder, Log Drains and SIEM integration are not implemented.

## Page integrity (detection only)

`POST /v1/firewall/integrity/report` accepts a bounded set of script
digests (64 lowercase hex) and records detected mismatches. This is
DETECTION only: no CSP `script-src` enforcement, no nonce injection, no
script inventory approval workflow and no browser blocking. Vercel's Page
Integrity is Beta on Pro/Enterprise Priority projects, uses CSP
Report-Only sampling and is separately billed; none of that is claimed.

## Measured geniee.site incident

The operator pasted 300 log lines labelled "301-600 of 601+ matched": a
BOUNDED SAMPLE, not the whole incident. Measured in that sample: 135 already
429, 80 distinct paths returned 307/404 (not blocked), 79 challenge-relevant
after excluding the legitimately public `/robots.txt`; a burst of 214
requests in 4.17 s with a peak of 72 requests in a rolling 1 s window. The
first 429 arrived ~1.7 s after the burst began, so the earliest probes
reached a redirect/404. Live replay against the real `hive-cloud` binary on
a fresh empty `HIVE_DATA` closes all 79 gaps with mode OFF (fixed 403) and
keeps them closed with mode ON. Corpus and witness outputs are in
`.gm/witness-log.md`.

## Not implemented / residuals

- L3/L4 volumetric absorption, anycast scrubbing and automatic attack detection.
- Vercel's proprietary bot-signal network, reverse-DNS verified-bot registry,
  JA4/JA3 TLS fingerprints, `x-vercel-bot-*` header semantics and BotID
  `checkBotId()` Deep Analysis (Kasada-powered).
- CSP script-src enforcement, `x-nonce`, script inventory approval and
  CLI `vercel page-integrity`.
- Vercel WAF custom-rule engine, `conditionGroup`, managed rulesets, IP
  Blocking, System Bypass Rules, WAF Rate Limiting rule shape and
  `vercel.json` `routes[].mitigate`.
- Firewall log retention/drill-down, Query builder, Log Drains, Firewall Alerts.
- Vercel Security Checkpoint browser attestation; ours is a JS-capability gate.
- Owner proxy / gossip dispatch arm / fleet fanout for the node-local event ring;
  a mode toggle is durably persisted but is not fanned out via
  `db_replicate::fanout_all` to every owner-chain candidate.
- `HIVE_SECRET_KEY_OLD` MAC fallback during key rotation; outstanding challenge
  cookies fail closed and must be re-issued (the same gap exists in the
  deployment-password unlock token).
- Triple-encoded traversal (`%25252e...`) survives the bounded double decode;
  static bypass prefixes beyond the listed asset classes.
- No 1:1 Vercel Firewall or BotID SDK equivalence. No challenge cookie name
  and no challenge HTTP status are documented by Vercel; `hive_fw` and the
  200 challenge page are platform-specific.

## Sources

- https://vercel.com/docs/vercel-firewall/attack-mode
- https://vercel.com/docs/vercel-firewall/ddos-mitigation
- https://vercel.com/docs/vercel-firewall/firewall-observability
- https://vercel.com/docs/vercel-firewall/page-integrity
- https://vercel.com/docs/botid/get-started
- https://vercel.com/docs/botid/verified-bots
- https://vercel.com/docs/botid/verify-on-page-load
- https://vercel.com/docs/botid/advanced-configuration
- https://vercel.com/docs/botid/form-submissions
- https://vercel.com/docs/bot-management
- https://bots.fyi (Vercel's public verified bots directory)
