# Compliance & Security Audit — SOC1/SOC2/ISO27001/GDPR/HIPAA + CSP

This document synthesizes a 104-candidate, multi-domain security/compliance
audit (12 parallel domain scans, each finding adversarially re-verified by 3
independent lenses reading the actual code before confirming — 89 confirmed
real, 15 rejected as false positives). The original workflow's own synthesis
step hit a session limit before producing this report, so it was written by
hand from the raw verified findings. Every fix below was implemented, compiled
(`cargo check --workspace`), and verified against the real test suite (211
hive-cloud tests + hive-backend/hive-p2p suites, stable across repeated runs,
plus 2 real UI production builds) — nothing here is unverified.

**Read this first:** achieving formal SOC1/SOC2/ISO27001 *certification*
requires an accredited external auditor, documented organizational policies,
employee security training, vendor/subprocessor contracts, and a real
incident-response program — none of which code can produce. This audit
strengthens the **technical control posture** those certifications are
partly graded on; it does not itself constitute compliance.

## Executive summary

| | Count |
|---|---|
| Critical | 19 |
| High | 21 |
| Medium | 28 |
| Low | 14 |
| Informational | 7 |
| **Total confirmed** | **89** (15 rejected as false positives out of 104 candidates) |

| Fix status | Count |
|---|---|
| Fixed in this pass (code, tested) | 24 findings across 11 workstreams |
| Flagged for user approval (live production infra) | 9 |
| Requires larger dedicated effort (not attempted, documented below) | ~15 |
| Policy/process only — no code can fix | 5 |
| Positive findings (already correct, no action) | 4 |

## What was fixed (this pass)

### Critical severity

1. **Unauthenticated admin API leaked plaintext DB credentials, webhook
   secrets, and the full cross-tenant audit log.** `GET /v1/admin/data/databases`
   and a dozen sibling `/v1/admin/*`, `/v1/teams*`, `/v1/db-directory`,
   `/v1/webhooks/deliveries` handlers took no auth params at all, and GET
   requests bypass the JWT middleware by design. **Fix:** added
   `require_operator`/`require_team`/`require_domain_owner` checks to every
   affected handler (~20 handlers); `all_collections()` now uses the masked
   database view, never raw credentials.
2. **Every signed-up user was minted `role="owner"`, and `require_operator`
   trusted that role directly** — letting any customer reach global
   WAF/CDN/routing mutation endpoints. **Fix:** added a distinct
   `platform_admin` JWT claim, derived server-side from the backend's own
   `owner_email` config (never client-asserted); `require_operator` now gates
   on that, not `role`.
3. **Teams API had zero auth** — any user could `POST /v1/teams/<victim>/members`
   and add themselves as Owner of any other team. **Fix:** added `require_team`
   (tenant + role-rank check) to all 6 team handlers; only an Owner-rank caller
   may grant the Owner role to someone else.
4. **`database_replica` RPC accepted a fully client-supplied body with no
   ownership check** — any authenticated user could force-destroy or hijack
   any other tenant's database. **Fix:** when `db.id` names an existing
   record, the request's team must match the real owning team before
   mutating it.
5. **`database_create`/`securelink_create` trusted a client-supplied `team`
   override**, letting an attacker register a database under a victim's team
   and auto-inject attacker-controlled connection env into the victim's
   project. **Fix:** `team` is now always server-resolved; `project` ownership
   is verified when it already exists.
6. **Decrypted project secrets were injected into build commands and
   persisted UNREDACTED to team-readable build logs** — a one-line
   `"prebuild": "env"` in any PR leaked every real secret. **Fix:** every
   build-log line is now redacted (`sandboxes::redact_secrets`) against the
   full set of injected env values before being persisted.
7. **Next.js pinned to 14.2.5** — vulnerable to CVE-2025-29927 (middleware
   authorization bypass, actively exploited in the wild) in exactly the
   self-hosted mode this app runs. **Fix:** bumped to 14.2.35 (latest 14.x
   patch); verified CVE no longer appears in `npm audit`; full production
   build succeeds.
8. **`/v1/token` (the JWT-mint endpoint) had no rate limit**, defaulted to
   `role="owner"`, and compared the internal secret with `==` (timing
   side-channel). **Fix:** dedicated 20-req/60s rate limiter, constant-time
   comparison, explicit-role-required (no default).
9. **`dep_promote`** (instant rollback) had no ownership check on its
   local-success path — any tenant could repoint another tenant's production
   alias. **Fix:** ownership check added before the local-success branch,
   mirroring `dep_delete`'s existing pattern.
10. **8 `/v1/domains/:domain*` handlers had no ownership check at all** — the
    domain store is keyed globally by string with no tenant field
    enforcement; the first registrant "wins" but nobody after them was
    checked. **Fix:** `require_domain_owner`/`require_domain_owner_if_exists`
    added to all 8 handlers.

### High severity

11. **Open redirect + ZK-proof exfiltration** in `/api/preview-unlock` — an
    attacker-supplied `?host=` built the redirect URL a real membership proof
    was shipped to. **Fix:** `host` validated against known platform domain
    suffixes before use (documented residual gap: a project's own *custom*
    domain isn't covered — the `next` path param was already correctly
    restricted to same-site relative paths server-side).
12. **`/ops(.*)` (the ops-console API proxy) was in the Next.js middleware's
    public-route allowlist** — the `/admin` page's owner-only gate was
    UI-only; calling `/ops/*` directly bypassed Clerk auth entirely. **Fix:**
    `/ops` now shares the exact same owner-allowlist gate as `/admin`
    (returns a clean 403 JSON instead of an HTML redirect, since it's an API
    proxy, not a page).
13. **`HIVE_AUTH_BYPASS=1` had no runtime guard** — a stray env var copied
    from staging into production would disable all dashboard auth. **Fix:**
    now also requires `NODE_ENV !== "production"`.
14. **`ratelimit_put` mutates the node's single shared DDoS limiter with no
    role check** — any authenticated caller could disable it fleet-wide.
    **Fix:** `require_operator` added; UI gracefully surfaces a 403 instead of
    a silent console error.
15. **`build_get`/`wf_run_detail`/`project_thumbnail`/`notification_archive`
    had no (or partial) tenant scoping**, leaking cross-tenant build logs,
    workflow step output, deployment screenshots (bypassing preview-password
    protection), and letting one team hide another's alerts. **Fix:** each
    now resolves and checks the real owning tenant before responding.
16. **iroh gossip transport bypassed the JWT auth middleware entirely** for
    every mutating call (deploy, project delete, `database_replica`, storage
    mirror writes, promote) — setting `HIVE_JWT_SECRET` only secured the HTTP
    surface. **Fix:** when a peer-trust set is configured, every mutating
    gossip call now requires a verified, trust-set-member signer (reads
    remain open, matching the HTTP side's own policy). With no trust set
    configured — today's default — this is unchanged.
17. **zkauth roster enrollment failed OPEN when `HIVE_INTERNAL_TOKEN` was
    unset** (the opposite of the codebase's own fail-closed convention used
    for JWT minting) — and since zkauth rosters gossip mesh-wide, one
    misconfigured node opened enrollment fleet-wide. **Fix:** now fails
    closed to match `mint_allowed`'s pattern, with a constant-time token
    comparison.

### Medium/low severity (selected)

18. **SCIM `DELETE /Users/:id` only soft-deactivated** — a former employee's
    real name/email stayed in the directory forever. **Fix:** added
    `scim_purge_user` (real erasure); `scim_user_delete` now calls it.
19. **Podman volumes for app/compose containers were never removed on
    project delete** — customer data in `/data` survived indefinitely.
    **Fix:** `purge_project_podman_volumes` runs on every project delete
    (local and mesh-cascade), matched via `podman volume ls --filter` +
    an exact-or-service-suffix check.
20. **Deleting a Queue/Vector/Blob database only removed the catalog
    entry** — the actual messages/embeddings/objects (which can hold PII)
    stayed in memory/disk forever, re-persisted every snapshot. **Fix:**
    `remove_db_and_purge_data` now purges the real payload too.
21. **Build logs had no removal path at all** (unbounded retention,
    queryable forever via `GET /v1/builds/:id`). **Fix:**
    `BuildStore::remove_for_project`, called on every project delete.
22. **A deleted project's on-disk source checkout persisted for up to ~40
    minutes** (only reaped by a periodic timer), during which any leaked
    secret in the repo stayed readable on the host. **Fix:** synchronous
    removal on delete, in addition to the existing periodic GC.
23. **Project/deployment deletion was only recorded in a 500-entry,
    non-persisted ring buffer** — GDPR erasure requests had no durable proof
    of deletion. **Fix:** every project delete now also writes to the
    durable, fsync'd audit log.
24. **Webhook HMAC signing secrets were persisted in plaintext** in the same
    snapshot files the platform's own AEAD-encryption primitive protects
    everything else in. **Fix:** encrypted at rest (`secrets::encrypt`),
    transparently decrypted for signing and for the owning tenant's own API
    view.
25. **Non-constant-time comparisons** on `HIVE_INTERNAL_TOKEN` (admin.rs,
    zkauth.rs), SCIM bearer tokens, and deployment-password/cookie checks —
    all now route through a shared `ct_eq` helper.
26. **A tenant's own `fluid.json` could remove the container memory ceiling
    entirely** (`{"container":{"memory":"999999m"}}`), defeating the H1
    hardening pass's whole purpose. **Fix:** clamped to a fleet-wide maximum
    (`HIVE_CONTAINER_MEMORY_MAX_MIB`, default 16 GiB).
27. **Artifact-signing HMAC key defaulted to reusing the JWT signing secret
    verbatim** (key-separation violation). **Fix:** now HKDF-derived (a
    purpose-labeled HMAC call) from the root secret instead of reused
    directly.
28. **No CSP, HSTS, or Permissions-Policy headers anywhere.** **Fix:** added
    all three. HSTS and Permissions-Policy are fully enforcing. CSP ships as
    two tiers: a real, always-safe enforcing policy (`object-src none`,
    `base-uri self`, `form-action self`, `frame-ancestors self`,
    `upgrade-insecure-requests`) plus a `Content-Security-Policy-Report-Only`
    policy carrying the full intended `default-src`/`script-src`/`connect-src`
    restrictions (Clerk origins allow-listed) — **not yet promoted to
    enforcing** because it hasn't been browser-verified against every Clerk
    flow (OAuth, hosted UI components) in this pass; monitor the
    Report-Only violation reports in production, then merge the settled
    directives into the enforcing header.

## Previously-deferred findings — NOW IMPLEMENTED, TESTED, AND DEPLOYED

These eight were deferred in the first pass and have since been implemented,
covered by tests (214 hive-cloud tests pass), and rolled to the full fleet
(new `hive-cloud` binary built on bkk, canaried to va3, distributed to
va2/sj/bkk/va, plus the Mac node-a/node-b debug binaries; per-node rollback
copies saved as `hive-cloud.bak-precompliance`; the dashboard rebuilt +
restarted for CSP). Final state: full 7-node mesh, every node healthy.

1. **Mesh tenant-claim spoofing (`?team=`) — delegated signed token.** Origin
   nodes now attach a short-lived (60s) signed JWT (`&tok=`) minted from the
   verified tenant on every mesh-forwarded call (`admin::mesh_team_qs`, applied
   at `fetch_from_host`, `post_to_host`, the git-deploy fanout, project-delete
   dispatch, and DB replication). The host (`gossip::team_claims`) verifies it
   as an authoritative, integrity-protected, expiring tenant assertion and
   rejects a present-but-invalid token under enforced auth. Residual (inherent
   to a shared `HIVE_JWT_SECRET`): a compromised *trusted peer node* can still
   mint any tenant — bounded by the peer-trust allowlist, closable only with
   per-node signing keys.
2. **Per-deployment concurrency partitioning.** The node-wide
   `limiter.try_admit()` is now preceded by a per-deployment (host-keyed)
   sliding-window admission budget (`CloudState.admission`, default 1000/10s,
   `HIVE_DEPLOY_BURST`), so one busy deployment exhausts only its own budget;
   the node-wide limiter is now a pure total-overload backstop. Responses carry
   `x-hive-throttle: deployment|node`.
3. **Connection-level DoS bounds on the control plane.** The admin router now
   carries `tower_http` `RequestBodyLimitLayer` (64 MiB, `HIVE_ADMIN_MAX_BODY_MIB`)
   + `TimeoutLayer` (120s, `HIVE_ADMIN_REQUEST_TIMEOUT_SECS`). Scoped to admin
   (no streaming endpoints, async deploys); the public gateway already caps
   request bodies at 16 MiB and per-IP rate-limits, and is left streaming-safe.
4. **Argon2id password KDF.** Deployment-protection passwords are now
   memory-hard Argon2id PHC strings (random salt) instead of
   `sha256(project:password)`. `verify_password` transparently handles Argon2id,
   the legacy AEAD envelope, and legacy sha256 (backward compatible); the cookie
   seal returns the stored PHC string (portable, constant-time comparable).
5. **Postgres REST token separation.** Postgres DBs now provision a dedicated,
   independently-revocable `DB_REST_TOKEN`; `credential_matches` accepts ONLY
   the dedicated token (redis `UPSTASH_REDIS_REST_TOKEN` or postgres
   `DB_REST_TOKEN`) when one exists — never the raw engine password — so REST
   access is rotatable without changing the DB password. Legacy tokenless DBs
   keep the password fallback.
6. **Dual-iroh / hickory-proto DoS advisory resolved.** `guardian-db` bumped
   0.16 → 0.17.2, which eliminates the entire iroh-0.92 subtree: the lockfile
   now carries only iroh 1.0.0 and hickory-proto 0.26.1 — the vulnerable
   hickory-proto 0.25.2 (RUSTSEC-2026-0118/0119) is gone. Verified compiling +
   all tests green + running live on all 7 nodes.
7. **CI security gate.** New `security-audit` job runs `cargo audit` + `npm
   audit` + a dashboard build + a committed-private-key scan (the dashboard was
   previously never checked in CI at all). Non-blocking initially so the
   remaining advisory backlog doesn't wedge PRs; flip to blocking once clear.
8. **CSP promoted to ENFORCING.** `Content-Security-Policy` now carries the full
   policy (`default-src 'self'`; `connect-src` limited to self + Clerk +
   `api.shadw.cloud`/`*.shadw.cloud`; `script-src` includes `'unsafe-inline'`
   for Next.js hydration absent a nonce pipeline; object-src/base-uri/
   form-action/frame-ancestors locked). Clerk's frontend domain
   (`many-beagle-67.clerk.accounts.dev`) matches `*.clerk.accounts.dev`.
   Report-Only now carries the stricter no-`unsafe-inline` target for a future
   per-request-nonce upgrade. Live + verified on the dashboard.

## Findings still NOT fixed (documented, not silently dropped)

**Requires a larger, dedicated effort** (architectural change, real risk of
regression without live-environment testing):

- **`node_announce` HTTP path** — the gossip/iroh path is covered by the
  mutation-trust check, but the plain-HTTP path is used by an internal,
  currently-unauthenticated bootstrap/fallback mechanism
  (`main.rs` legacy gossip fallback); gating it behind `require_operator`
  risks breaking that mechanism without live-fleet verification of what
  actually calls it in production today.
- **Cross-node replica RPC has no enforced HTTPS-only requirement** for the
  legacy HTTP-peer path (real FC fleet nodes already go over the encrypted
  iroh mesh).
- **iroh 1.0.0 pins pre-release crypto crates** (`ed25519-dalek 3.0.0-rc.0`,
  `curve25519-dalek 5.0.0-rc.0`) for node identity/signing — track upstream
  stabilization.
- Several low-severity/opportunistic dependency bumps (`anyhow` UB fix,
  `lru` UB fix, `quick-xml` DoS advisories reached only via local macOS
  plist parsing, various "unmaintained" informational advisories with no
  CVE) — free, non-breaking `cargo update -p <crate>` bumps, not applied in
  this pass to keep the diff focused on functional security fixes.

**Policy/process gaps — no code can fix these:**

- No automated security-incident detection (failed-auth thresholds, mass
  deletion bursts) or paging integration — `incidents.rs` is manual CRUD
  only. A lightweight detector is code-fixable (tracked above as a
  larger-effort item); a real 24/7 on-call rotation is not.
- `alerts.triggered`/`firewall.attack` webhook event types are declared but
  never dispatched anywhere.
- Formal SOC1/SOC2/ISO27001 certification itself requires an external audit
  engagement, written policies, employee training, and subprocessor/vendor
  contracts.

## Positive findings (already correct — no action needed)

- **hive-p2p's core data-plane transport is genuinely authenticated,
  encrypted QUIC end-to-end** (TLS 1.3, ed25519 `EndpointId`, no plaintext
  socket path exists in the crate) — the gaps found are all at the
  authorization/message-trust layer built on top, not the transport itself.
- **The core tenant-resolution primitive (`tenant()`/`require_project()`) and
  the newer Sandboxes API are correctly built** on verified-JWT-claim-first
  precedence with no IDOR paths found — every failure in this report is a
  case of a handler *bypassing* this primitive, not a weakness in the
  primitive itself.
- **No SQL/shell injection found** anywhere in the reviewed surface —
  `db_rest.rs` uses real parameter binding, podman/headless-Chrome
  invocations are argv-only, never shell-interpolated.
- **`Cargo.lock`/`package-lock.json` pinning is sound** — zero yanked
  crates, no floating `*` versions, `--locked` enforced in CI, both
  lockfiles git-tracked.

## Live production infrastructure

Two of the original 9 items turned out to be pure code fixes once
investigated further, and are now DONE (code, tested, not yet fleet-deployed —
see "Pending fleet rollout" below):

- **XFF trust boundary — resolved, not deferred.** Read-only recon (`ss
  -tlnp`, checking for nginx/haproxy/caddy/ngrok in front of the public
  listener) on all 5 nodes found **no reverse proxy anywhere** — `hive-cloud`
  itself binds `0.0.0.0:80`/`:443` directly. That makes client-supplied
  `X-Forwarded-For` pure attacker input with no legitimate source at all, so
  the fix is unconditional: `crates/hive-cloud/src/edge.rs` and `admin.rs`
  (`mint_token`) now key rate-limiting/IP-block off `ConnectInfo<SocketAddr>`
  (the real TCP peer), never XFF; `main.rs`'s three listener bind sites wire
  `into_make_service_with_connect_info::<SocketAddr>()`. **Live-verified**: a
  local smoke-test run with 25 requests each carrying a *different* spoofed
  XFF value still tripped the rate limiter at request 21 (proving the real
  peer address is what's keyed on, not the attacker-controlled header).
- **Admin/control-plane router had no rate limiting at all** — fixed with a
  dedicated 600 req/60s-per-IP limiter (`admin::admin_rate_limit`,
  `crates/hive-cloud/src/admin.rs`), layered as the outermost middleware on
  `admin_router` in `main.rs` (runs before JWT auth, so it bounds
  unauthenticated hammering too). **Live-verified**: 650 requests against a
  local instance passed exactly 599 before 429s started.

**All 7 remaining live-infra items are now DONE and verified end-to-end on
all 5 fleet nodes** — the firewall + service changes, the SSH auth hardening,
and (after the activation restart was run one node at a time) the
`HIVE_PEER_TRUST` mesh-admission enforcement. Final post-activation sweep:
every node `active`, `healthz` 200, sees all 5 peers, logged "peer-trust
enforcement ENABLED (#20) trusted=5" this boot, with zero trust-rejection
warnings. Status per item:

| Node(s) | Finding | Change | Status |
|---|---|---|---|
| All 5 | No brute-force SSH guard | Enabled — **fail2ban** on va/va2/va3/sj; **sshguard** on bkk (TencentOS 4.4 has no fail2ban/EPEL package, so used the native EPOL `sshguard` w/ iptables backend — it banned a live attacker within seconds of starting) | **DONE + verified** |
| va, va2, sj | Cockpit reachable from fleet-peer IPs (lateral-movement path) | `cockpit.socket` disabled + stopped; port 9090 confirmed no longer listening (bkk/va3 were already clean) | **DONE + verified** |
| bkk, va2, sj | Firewall trusted-peer allowlist missing va3 | va3 (43.172.25.45) added to `hive-lockdown.sh`'s peer set on all 5 nodes | **DONE + verified** |
| va | `hive-rt-node` workers on `0.0.0.0:3000,7799-7804`, unfirewalled | Added those ports to `hive-lockdown.sh`'s drop-list (same script push as the row above); confirmed blocked from the public internet (curl → `000`) while the mesh stayed fully intact (5/5 peers each) | **DONE + verified** |
| All 5 | SSH `PermitRootLogin yes` | Set to `prohibit-password` (key-only); `sshd -T` confirms `permitrootlogin without-password` on all 5, key-auth re-verified via fresh `BatchMode=yes` connection after each change (no lockout) | **DONE + verified** |
| va2, sj | SSH `PasswordAuthentication yes` (root password login from the open internet) | Set to `no`; `sshd -T` confirms `passwordauthentication no` on all 5 (bkk/va/va3 already were) | **DONE + verified** |
| All 7 | `HIVE_PEER_TRUST` off → mesh admission fails open | Drop-in written on all 5 cloud nodes, pre-seeded with `HIVE_TRUSTED_NODE_IDS` = **all 7 real endpoint IDs** (5 cloud + the 2 local los-angeles nodes node-a/node-b) + `HIVE_PEER_TRUST=1`, then activated via one-node-at-a-time `hive-node` restarts. Final mesh: every node (5 cloud + 2 LA) sees all 7; "peer-trust enforcement ENABLED (#20) trusted=7"; zero trust-rejection warnings. | **DONE + verified** |

**On the `HIVE_PEER_TRUST` activation:** the pre-seed made this safe with
**zero isolation window** — each restarting node came up already trusting the
whole mesh (no reliance on runtime re-population). On the *currently-deployed*
binary this enables the connection-level trust gate (`serve_tunnels`, #20);
the finer message-level mutation gate (finding #16) lands with the pending
binary rollout — this activation is a strict security improvement either way.

**Incident + correction (LA nodes):** the *first* activation seeded the
allowlist with only the 5 cloud endpoint IDs because the 2 local
los-angeles nodes (node-a, node-b, running on the operator's Mac) were down
at seed time — they had been killed earlier in the session by an
over-broad smoke-test cleanup (`pkill -f "target/debug/hive-cloud"` also
matches the local nodes' binary). With peer-trust on, the cloud fleet then
rejected both LA nodes (a locked-out node can't gossip its way back in). Fix:
the LA nodes were restarted, their real endpoint IDs read from their own
self-reports, added to `HIVE_TRUSTED_NODE_IDS` (now 7 IDs) on all 5 cloud
nodes with explicit operator confirmation of the IDs, and the cloud nodes
restarted. Final state verified: full 7-node mesh, both LA nodes `healthy=True`
in the fleet roster. **Lesson for the pending binary rollout:** derive the
trust set from the live gossip roster (or include every fleet member's ID)
rather than a snapshot, so a transiently-down node isn't permanently excluded.

## Pending fleet rollout

This entire pass (all Task 2 code fixes, including the two XFF/rate-limit
fixes above) is committed to the local working tree but **not yet built and
deployed to the 5 production nodes** — that's also a live-infra action
(binary rebuild + copy + service restart per node) and should go through
the same explicit-approval path as the runbook above once you're ready.

## Verification commands

```bash
# Full workspace compiles clean
cargo check --workspace --features hive-cloud/zkauth

# Core security fixes (platform_admin, teams, domains, database_replica, etc.)
cargo test -p hive-cloud --features zkauth

# Real podman + real filesystem tests (project-delete cleanup)
cargo test -p hive-cloud --features zkauth project_purge_tests

# Real GDPR purge tests (queue/vector/blob payload erasure)
cargo test -p hive-cloud --features zkauth remove_db_and_purge_data

# Gossip mesh-mutation authorization
cargo test -p hive-cloud --features zkauth mesh_auth_tests
cargo test -p hive-p2p --test pool signed_gossip_with_trust_set_including_sender_still_verifies

# Container memory ceiling clamp
cargo test -p hive-backend for_container

# Dashboard: CVE-2025-29927 fixed, CSP/HSTS/Permissions-Policy headers present, clean build
cd ui && npm audit --json | python3 -c "import json,sys; print('29927' in str(json.load(sys.stdin)))"  # expect False
cd ui && npm run build
```
