# Changelog

## (pending) — Browser functions run against a real Node API

A browser function may now be written the way a Node function is written. The
build-time scan that rejected `require(`, `import(`, `process.`, `Buffer`,
`__dirname`, `__filename`, `Deno.`, `Bun.` and (in quickjs mode) `fetch(` is
GONE — globally, for every function — and the substrate supplies the surface
those tokens name instead of refusing the source that used them.

- **`crates/hive-browser/www/node-runtime.js` (new).** Installed in the guest
  immediately before the artifact function: a CommonJS `require` over the Node
  builtin set (`path`, `events`, `util`, `stream`, `string_decoder`,
  `querystring`, `url`, `os`, `assert`, `crypto`, `fs`, `http`/`https`,
  `buffer`, `process`, `timers`, `perf_hooks`, `vm`, `module`, `async_hooks`,
  `zlib`, `v8`, …) plus `express`, a real `Buffer` (a `Uint8Array` subclass),
  `process`, `console`, timers, `URL`/`URLSearchParams`,
  `TextEncoder`/`TextDecoder`, `atob`/`btoa`, `performance`,
  `structuredClone`, `AbortController`, `__dirname`/`__filename`. The QuickJS
  guest had NONE of these — measured against the shipped bundle, its global
  object is bare ECMAScript, so the old rejection message ("use
  Uint8Array/TextEncoder, both exist in QuickJS") named an API that was not
  there.
- **Not almostnode.** The obvious candidate is a 16 MB browser-hosted Node
  emulator that owns module resolution, an npm client and dev servers, executes
  with host-realm `eval`, assigns `globalThis.process`, and references
  `window`/`document`/`navigator`/`localStorage`/`Worker`/service workers
  throughout its built bundle — none of which exist in a QuickJS guest, and its
  own README rates `net`/`tls`/`dns`/`dgram`/`cluster`/`vm`/`v8` "stubs only"
  and tells you to run untrusted code in a separately-deployed cross-origin
  iframe. What it genuinely provides for a sandboxed guest is implemented
  directly here, against this substrate's real primitives.
- **Both calling conventions, one reconciliation point.** The emitted wrapper
  passes the handler a `req` that is a SUPERSET of the platform request
  descriptor and a `res` that is a superset of `ops`, so
  `(request, ops) => ({status, body})` keeps working byte-identically while
  `(req, res) => res.json(...)`, an Express app, and
  `http.createServer(...)` + `server.listen(PORT)` all work as written.
  `bridge.settle(out)` decides: an explicit non-`res` return value wins,
  otherwise the response written on `res` is used (including the
  `return res.json(x)` shape, which returns `res`, not `undefined`).
- **Unsupported stays LOUD, at the honest boundary.** `net`, `tls`, `dns`,
  `dgram`, `cluster`, `child_process`, `worker_threads`, `zlib`, outbound
  `http.request`, `crypto.randomBytes`/`randomUUID` (there is no CSPRNG in the
  guest, and Math.random is not one), a relative `require('./x')` and any npm
  dependency each throw a NAMED error at the call — never a silent no-op. The
  refusal moved from build time to the exact line that cannot work, so a
  handler that never takes that branch is no longer blocked from deploying.
- **`process.env` is EMPTY** (`NODE_ENV`, `HIVE_BROWSER_NODE` only) and is never
  populated from the host: project env and secrets still never ship to a
  donor's browser. That was the real reason `process.` was banned; it is now
  enforced where it belongs.
- **Unchanged on purpose:** the canonical policy digest (both implementations,
  byte-identical, no new mode and no new host op), admission/capability
  derivation, tenant ownership checks, and pin()'s size + BLAKE3 + policy-digest
  verification of artifact bytes. The Node runtime wraps AROUND verified source;
  it never rewrites it and grants nothing `allowed_ops` did not already grant.
- **Still rejected at build:** static `import`/`export` STATEMENTS. The artifact
  is evaluated as one function expression, where they are a hard SyntaxError —
  permitting them would only defer the failure into every donor's browser, at
  boot, for every request. CommonJS is the supported form.
- Published: `ui/scripts/sync-browser-node.mjs` now copies `node-runtime.js`
  into `ui/public/browser-node/` — it is a STATIC import of
  worker-function-runtime.js, so omitting it would break the SharedWorker's
  whole module graph on the fleet while every local check stayed green.

## (pending) — Browser nodes are dispatched to, not hand-fed

A running browser node now receives whatever browser-eligible work its tenant
has, automatically, instead of serving the one artifact a human picked at
start. The picker survives as a deliberate override, not a gate.

- **The admission capability is a SET.** `browser_admission::validate_request`
  resolves the donor's whole eligible set server-side
  (`browser_artifacts::eligible_for_tenant`: every browser-eligible function of
  every Ready deployment under the AUTHENTICATED tenant, from the same two
  replicated sources `descriptor_for` already reads, deterministically ordered
  production-then-newest and capped by `HIVE_BROWSER_AUTO_SERVE_MAX`, default
  16). It is re-derived on every renewal, so a function deployed after the node
  started is served within one lease tick — no restart, no re-pick. The
  capability block gains `artifacts[]` and keeps mirroring its first entry into
  the flat fields a pre-upgrade worker reads.
- **`serve_mode` is a request, not a capability.** `"auto"` asks the server to
  derive the set; absent (a pre-upgrade worker) or anything else still means
  serve nothing, so a rollout never starts serving code on a donor that did not
  ask for it. Naming a `deployment` overrides auto and pins exactly that one —
  today's behaviour, unchanged.
- **One endpoint, many routes.** `fluid_gateway::set_browser_targets` replaces
  an endpoint's complete registration set atomically (one entry per function
  key, each validated exactly as before, hard-capped at
  `MAX_BROWSER_TARGETS_PER_ENDPOINT`); `upsert_browser_target` is now its
  one-element wrapper. `routing_identity_changed` became a superset test: a
  pure addition no longer tears down the browser's QUIC trunk and constellation
  presence every time anyone in the tenant deploys.
- **The scalar admission triple is now a compat view only** and is kept a
  coherent member of the set — never a deployment-A/function-of-B mixture,
  which a pre-`serves` follower would have registered as a route that could
  execute the wrong digest for a same-named function. In auto mode with no
  database pin it is empty, so an old follower routes nothing rather than
  something wrong.
- **Databases are not auto-picked among several.** A browser holds one replica,
  so `browser_db::auto_db_deployment_for_tenant` grants only when the tenant has
  exactly one project carrying a `browser_db` block; with two or more the picker
  chooses or the node runs without one.
- **Worker (HOST_ABI v4).** `normalizeCapability`/`reconcileCapability` handle
  the descriptor set: pin every authorized artifact (both BLAKE3 digests
  recomputed locally, cache-first), then revoke stale digests/callers before
  granting replacements. A per-artifact failure no longer discards the rest —
  it lands in `status.functions.failed` and retries next renewal; only a total
  failure takes the old backoff path. `status.functions.serving[]` names what is
  actually pinned, so /run-node reports the real set instead of a count.

## (pending) — Durable deployment roots + concurrent same-project zip deploys

Two deployment-lifecycle fixes in the git/zip build path, both live-witnessed
on a local `HIVE_FORCE_MOCK=1` node:

- **Deployment roots are durable (`$HIVE_DATA/deploys`, not `/tmp`).** The mock
  backend serves files straight from a deployment's recorded `root` for its
  whole life, but `git::deploy_root()` was `$TMPDIR/hive-deploys` — a host
  reboot wiped the checkout while the replicated deployment RECORD survived,
  so the node 404'd `DEPLOYMENT_NOT_FOUND` for a deployment it believed it had
  (root-caused live: dan.shadw.app, 2026-08-03). New checkouts and retained
  zip sources now live under `$HIVE_DATA/deploys`; boot restore is unchanged
  (records carry absolute paths), so pre-upgrade `/tmp` roots keep serving
  until their host reboots, and every root-scanning reader
  (`newest_deploy_dir`, `gc_build_dirs`, `purge_project_source_dirs`,
  retained-source lookup) falls back to the legacy `/tmp` root — a pre-upgrade
  zip project still redeploys from its `/tmp`-retained archive and
  re-materializes into the durable root (witnessed). The project-name path
  component is now `sanitize_tag`'d everywhere a checkout dir is built
  (tenant-controlled text is never a path component verbatim), with
  raw-name prefix fallback (`git::checkout_prefixes`) so pre-sanitization
  checkouts remain visible to redeploy/GC/purge.
- **Concurrent same-project builds no longer share one checkout dir.**
  `run_build` named the checkout `<project>-<now_ms()>`, and two concurrent
  builds (both woken by the same timer tick from the synchronized 350ms
  pre-build sleep) land on the SAME millisecond far more often than intuition
  says — one shared dir, two racing `unzip -o` processes, and the loser dies
  with exit 50 "cannot create …: No such file or directory" (reproduced both
  through the API and with bare `unzip`; the witness hit it 3x and had to
  serialize deploys). The checkout dir, the "Building…" placeholder dir, the
  extract temp zip, and the build-cache temp tar now all carry the build id;
  the retained `<tag>.src.zip` write is tmp+rename (two concurrent writers
  could previously tear it mid-read by a redeploy). Two concurrent zip
  deploys of one project now BOTH reach `ready` (witnessed 3/3 race-window
  hits, including one pair with byte-identical ms stamps); the alias resolves
  to the later finisher, matching Vercel's concurrent-deploy model. When the
  two requests instead serialize on the project-name check, the loser still
  gets the pre-existing loud 409 — retryable, never a silent strand.

## (pending) — Browser↔fleet live CRR exchange (browser_db databases replicate for real)

A project that opts in via a fluid.json top-level `browser_db` block now gets a
REAL replicated database, not just a contract: browsers holding a live,
server-derived grant sync divergent writes both ways against a per-project
fleet replica over one new `Op::CrrSync` op on the existing `hive/browser/0`
ALPN, riding the repaired hive-crsql seam (per-origin-site durable watermarks,
HCB1 canonical batches, gap/replay, transactional apply). Each request/reply
pair is a full bidirectional anti-entropy round: the request carries the
sender's watermarks (the responder's export selector) plus its push batches;
the reply carries a typed apply status (ok / sync-gap / quota-exceeded /
value-too-large / read-only / batch-refused), the responder's post-apply
watermarks (the acknowledgement the sender persists as its push cursor), and
the responder's bounded export — `more` means re-request, and the freshly
applied watermarks ARE the resume cursor. Both directions of initiation work:
browser-dialed rounds (wasm `crrSyncOn` → hive-p2p `serve_browser_conn` →
`browser_db::sync_round`) and fleet-dialed pulls (`BrowserPool::crr_sync`,
exposed for operators as `POST /v1/browser/dbs/sync/:endpoint_id`, metadata
only — DB bytes never leave the CRR protocol). Grants ride the admission: a
server-derived `db` capability block (`tenant, project, access, max_bytes,
max_value_bytes, db_file, schema, sync_peers, expires_ms`) plus a replicated
`BrowserAdmission.db` grant the exchange re-checks on EVERY request against
its own admission view AND the live descriptor (Public scope additionally
requires the live spec's `public_read`; foreign-tenant/unknown/block-removed
are the identical refusal). Caps enforce with typed refusal + whole-batch
rollback, never truncation; `BrowserDbPolicy.schema` (`{name, ddl}`) is how
both halves derive schema cr-sqlite doesn't replicate. Replica files are
platform-templated (`$HIVE_DATA/browser-dbs/hive-browserdb-{sanitize_tag(
project)}.db`), GC'd with the browser_artifacts blast-radius guards and a
30-day inert grace. Witnessed live on a real two-process setup (local
hive-cloud with enforced JWT + embedded relay, real Chrome tab running the
sqlite DedicatedWorker against OPFS): bidirectional convergence both ways
through the wire op, fleet-initiated pull, reload resuming from durable
watermarks with zero re-push, hand-crafted gap refused typed with no partial
state, oversized/quota pushes refused typed with rollback, revocation cutting
replication + OPFS wipe, and the pre-change-binary refusal contrast
(`HIVE_BROWSER_DB_LISTEN=0` → NO_HANDLER). See
`docs/browser-db-contract.md` and AGENTS.md's browser-replicated-databases
section.

## (pending) — Auto-deploy webhook-less git projects by polling the tracked branch

`git push` silently never deployed for any project imported as a plain public
repo URL (or whose owner never completed the GitHub connection): those have
`git_ci == None` and no webhook, so GitHub never notifies the platform and the
sole auto-deploy trigger (`admin::git_webhook`) never fires — with no visible
error (no failed delivery, because no webhook object exists). This was the exact
break for `shoomoo` (public repo `fatbearsk/serverless-clawdbot`, branch
`xstate`: deployed `664328b` while HEAD was `e80e18c2`). Added
`git::spawn_git_poll_reconcile`, a leader-only reconciler that polls each
git-sourced project's tracked-branch HEAD with `git ls-remote` (host-agnostic,
no GitHub REST rate limit, no credential for a public repo) and starts the same
build the webhook would whenever HEAD has advanced past the deployed commit.
Per-project SHA dedup (`CloudState::git_poll_seen`, seeded from the deployed
commit) makes a push deploy exactly once and never lets the poller and a real
webhook double-fire. Witnessed live: on rollout the leader auto-deployed six
previously-stuck webhook-less projects, each exactly once, and left already-
current projects (including the just-caught-up `shoomoo`) untouched.

## (pending) — Fix GitOps sync silently reading the wrong tenant + unbounded hang

Production GitOps sync intermittently showed "GitOps sync failed / Failed to
fetch" with a Retry button, even for tenants with a genuinely linked repo. The
prior "missing or invalid bearer token" fix (below) covered every *mutation*
through `gitops-server.ts`'s shared `backend()` helper, but left its *reads*
unauthenticated: `ui/app/api/gitops/sync/route.ts`'s own `/v1/gitops` link
lookup and all 10 concurrent `backend()` calls inside `buildOrgArtifacts`
(`/v1/teams/:team`, `/v1/gitops/projects`, `/v1/overview`, etc.) omitted the
caller's platform JWT. Under JWT enforcement, `admin.rs`'s `tenant()` resolves
any request with no claims to `ANON_TENANT` rather than the real caller — so
the link lookup silently read the (empty) link for `"__anon__"` and returned
`{skipped:true, reason:"no-config-repo"}` even when the tenant had GitOps
linked, and the `/v1/teams/:team` read hit `require_team`'s 403. `backend()`
now threads `authToken` through every read too, and `buildOrgArtifacts` takes
an `authToken` parameter forwarded by both of its callers
(`api/gitops/sync`, `api/gitops/init`) so every request in the chain resolves
to the real tenant.

Separately, `backend()` had no timeout: a single hung backend or GitHub API
call anywhere in `buildOrgArtifacts`/`commitFiles` could stall the whole
`/api/gitops/sync` request indefinitely, which is the shape of failure that
eventually surfaces client-side as a raw "Failed to fetch" rather than a clean
HTTP error. `backend()` now wraps its `fetch()` in an `AbortController` with a
20s timeout, matching `lib/api.ts`'s `fetchWithTimeout` default, so a hang
degrades to a retryable HTTP timeout that `gitops.tsx`'s existing retry/
backoff logic already handles. Witnessed: `cargo check -p hive-cloud` and
`npm run build` (ui/) both compile clean with these changes.

## (pending) — Fix build status stuck at "0 lines, Waiting for logs…" forever

A fresh deployment's progress page (`/deploy/[id]`) could get stuck showing
"Deployment started 0s ago…" with 0 log lines forever, even though the build
had genuinely started (and often finished) on the backend. Root cause: a
deploy mutation (`POST /v1/git/deploy`) is always forwarded to the current
control-plane leader by `admin_ingress`, so a fresh build frequently lives
only on the leader's in-memory `BuildStore` — but status *reads*
(`GET /v1/builds/:id`) are served best-effort local and are never forwarded.
A dashboard poll landing on a different (non-leader) fleet node than the one
that ran the build had no way to find it, and 404'd on every single poll
forever. `build_get` now mirrors the fallback its sibling `deployment_build`
already had: on a local miss, if this node isn't the control-plane leader, it
proxies the read to the leader via `fetch_from_host` before giving up with a
genuine 404. Witnessed: reproduced the exact stuck state live (a build
present on one fleet node, invisible via `GET` from a peer node), confirmed
the fix compiles clean and the genuinely-unknown-id case is unaffected.

## (pending) — Fix "missing or invalid bearer token" on private-repo deploys

Deploying (or redeploying) a repo through the dashboard failed with
`missing or invalid bearer token`. The deploy is POSTed to a same-origin Next
server route (`/api/git/deploy`) — not straight to the backend — so the route can
attach the user's GitHub clone token server-side. But the shared `backend()`
helper forwarded only `x-hive-team`, never the user's platform JWT: unlike the
`/cloud` rewrite proxy, a server→backend `fetch()` doesn't carry the browser's
`hive_jwt` cookie, so the backend's `require_auth` rejected the POST. `backend()`
now forwards the caller's platform token (via `authTokenFrom(req)`, which reads
the httpOnly `hive_jwt` cookie or an incoming `Authorization: Bearer`), and every
server-route mutation that goes through it — deploy, redeploy, and the GitOps
init/sync/integrations-link writes (the same latent class bug) — passes it
through. The GitHub *clone* token (`git_token`) is a separate credential and is
untouched; both are server-side only. Witnessed: the deploy that returned 401
now returns a `build_id`, and the no-token path is unchanged.

The same audit found the marketplace toolkit-index cache (`/api/composio/toolkits`)
silently failing its `guardian-db` write for the same reason (operator-gated
admin-data endpoint, no token) — so under enforcement it never persisted and
re-fetched Composio's full 1,000+ catalog on every Integrations load. It now
mints a short-lived `_global` operator token (server-side, via the internal
token + owner email) for the read and write, so the index caches (witnessed:
consecutive loads now serve `source: guardian-db`).

## (pending) — First-party GitHub App OAuth (private repos & orgs for projects/deployments)

Private organization repos still couldn't be used for projects and deployments:
the Composio-managed OAuth app can't grant org access (no `read:org`, and orgs
with OAuth-app restrictions had to approve a third-party app). The platform now
has its OWN GitHub App (org-level permissions) wired end-to-end. `POST
/api/github/connect` returns the first-party
`github.com/login/oauth/authorize` URL (HMAC-signed state, CSRF-bound to a nonce
cookie); GitHub redirects to the registered `/oauth/github/callback`, which
exchanges the code and seals the user token into an AES-256-GCM-encrypted
httpOnly cookie — the token is never stored server-side, never readable by the
browser, and auto-refreshes (expiring-token Apps) or is cleared on refresh
failure so status stays honest. A new `lib/github` facade fronts every GitHub
operation (repos, orgs, org repos, repo creation, webhooks, Actions variables,
GitOps commits, and the deploy/redeploy clone token) preferring the App token
with direct GitHub REST calls, and falling back to the existing Composio
connection unchanged — existing users are unaffected, and with neither
configured everything degrades exactly as before. Org access is now an App
*installation*: the Integrations GitHub card links "install the GitHub App on
the organization" (from the user's installation `app_slug`), and an
org-not-installed 403 threads the org's installations-settings URL through the
existing restricted/approve UI. Disconnect revokes the grant on GitHub and
clears the cookie. Witnessed live: credential validity, all callback error
paths, cookie crypto roundtrip + tamper rejection, expired-token refresh
failure clearing, Composio fallback, and the secret appearing in zero tracked
files.

## (pending) — Responsive lazy-loading, ISR, Speed Insights, image optimization & LLM SEO

The dashboard shipped none of the Vercel-style performance/discovery practices in
a way that was actually present on mobile: the root layout was `force-dynamic`, so
**every** route rendered dynamically (zero ISR, even the public marketing/docs
pages), most routes had no loading skeleton, images were raw `<img>` (no
responsive/modern-format optimization), the Speed Insights view had no in-app
collector, and there were no SEO artifacts for search or AI crawlers. This applies
all of them across the board, responsively.

- **ISR**: the root `force-dynamic` is removed. It was only ever needed for the
  home route's landing↔dashboard flip, which (like all auth chrome) is
  client-side — no page reads server auth (`auth()`/`cookies()`/`headers()`), so
  it was safe to lift. The home route keeps `force-dynamic` via a thin server
  shell (`page.tsx` → `home-client.tsx`) so Clerk SSR still avoids the flash;
  every public marketing + docs page is now `force-static` + `revalidate: 3600`
  (real ISR, `○`/`◐` in the build), and the per-user dashboard pages render a fast
  static shell that fetches data client-side.
- **Lazy loading**: a responsive `loading.tsx` skeleton (`PageSkeleton`) now
  covers every route (66 added), so navigation shows an instant, layout-stable
  placeholder on mobile; the heaviest client components (React-Flow graphs, Tremor
  charts, the command bar) stay `next/dynamic`-split.
- **Image Optimization**: all 25 raw `<img>` became `next/image` (responsive
  `srcset`/`sizes`, AVIF/WebP via `next.config` `images`, lazy by default,
  `priority` only on the two LCP logos) — arbitrary external-host avatars/logos
  stay hardened lazy `<img>`. Cumulative Layout Shift measured 0.
- **Speed Insights**: a self-contained Core Web Vitals beacon (`VitalsBeacon`) in
  the root layout collects FCP/LCP/CLS/INP/TTFB on every device and posts to the
  vitals sink, so the Observability → Speed Insights view reflects the dashboard's
  own real-user performance.
- **SEO for LLMs / AI search**: added `robots.ts` (welcomes GPTBot, PerplexityBot,
  ClaudeBot, Google-Extended, …), `sitemap.ts`, `llms.txt`, schema.org JSON-LD
  (Organization/WebSite/SoftwareApplication), a dynamic OpenGraph image, and
  unique per-page `metadata` (title/description/OG/canonical) with one `<h1>` per
  public page.
- **Responsive**: an explicit `width=device-width, initial-scale=1,
  viewport-fit=cover` viewport (zoom left enabled for accessibility). Verified no
  horizontal overflow at 375 / 768 / 1280 px.

## (pending) — GitHub connection management (scopes, orgs, reconnect, disconnect)

Users who connected GitHub for a private org or repo had no way to see what the
connection actually granted, re-authorize with adjusted scopes, request
organization approval, or disconnect and reconnect — and the platform made two
silent mistakes: it swallowed GitHub's org OAuth-app-restriction `403`s (a
restricted org's repo list just came back empty), and it treated
revoked-but-still-`ACTIVE` Composio connections as connected, showing a false
green. The Integrations page's GitHub "Configure" and "Disconnect" buttons were
dead no-ops.

The connection is now honest and fully manageable. `githubConnectionDetail`
reports `connected` only when the token is BOTH active in Composio AND live
against GitHub (a `GITHUB_GET_THE_AUTHENTICATED_USER` probe unmasks dead-`ACTIVE`
tokens and drops the cached token), and surfaces the granted scope names, the
account login, and whether private-repo (`repo`) and org-enumeration (`read:org`)
access are present — never a token value. New helpers and routes: `githubOrgs` +
`GET /api/github/orgs` (the user's orgs, degrading to `[]` without `read:org`);
`disconnectGithub` + `POST /api/github/disconnect` (deletes every connected
account for the user, so a later reconnect binds fresh); reconnect = disconnect
then re-run OAuth; `githubOrgRepos` now requests `type:all` and returns
`{repos, restricted, approve_url}` on an org OAuth-app restriction, via a shared
`orgApproveUrl` that parses GitHub's 403 body and falls back to the org's
third-party-apps policy page (never an empty link), also used by repo creation;
and `githubAuthConfigId` honors a `HIVE_GITHUB_AUTH_CONFIG_ID` pin, otherwise
requesting `[repo, read:org, workflow]`. The Integrations page gains a real
GitHub card (login, scope badges, accessible orgs, a red reconnect-needed banner
when the token is dead, and wired Disconnect / Reconnect-adjust-access /
Set-up-GitOps buttons); the GitOps modal swaps the free-text org field for a
dropdown with an approve link and a Re-check-access button and can be re-opened
on demand; and New Project merges organization repos into the import list.
Because Composio's managed GitHub app does not grant `read:org`, org
*enumeration* degrades gracefully and only prompts a (non-disruptive) reconnect
when a user actually needs it — private-repo access, which already works via
`repo`, is never interrupted. Verified live on all 7 fleet nodes: enriched
status serves, a dead-`ACTIVE` entity reports `live:false`, `/api/github/orgs`
and `/api/github/disconnect` are registered, and no response carries a token.

## (pending) — Deploy private GitHub repos (inject the connected-GitHub token)

Deploying a private GitHub repo failed with `fatal: could not read Username for
'https://github.com'` — the build's `git clone` ran anonymously and no layer ever
attached a credential. Now the user's connected-GitHub token is plumbed to the
clone: `GitDeployRequest` carries a `git_token` (fetched server-side by new
`/api/git/deploy` + `/api/projects/[project]/redeploy` routes from the user's
Composio GitHub connection, never exposed to the browser), and the backend injects
it as an `x-access-token` clone URL (github.com-only), with the token scrubbed from
clone stderr, an anonymous retry if a stale token is rejected (so public repos are
never broken), actionable no-credential vs rejected-credential error messages, and
the token cleared after the clone. A node-level `GITHUB_TOKEN` still works as a
fallback (e.g. for webhook auto-deploys). Verified live on the fleet; public repos
unaffected.

## (pending) — Usage page defaults to Monthly + lazy charts; fix flaky capacity test

The dashboard Usage view now selects the **Monthly** granularity on first load
(was Daily). The page is split into a server shell (`page.tsx`, carrying an ISR
`revalidate` config) and a client `usage-view.tsx`; the Tremor charts stay
code-split via `next/dynamic` and now render loading skeletons so the deferred
chunk never leaves a layout-shifting gap. (ISR note: the root layout's
`force-dynamic` — required for auth-correct chrome — currently supersedes per-page
static rendering app-wide, so `/usage` still renders dynamically; the ISR config
is kept forward-compatible and, since the page carries no server data, caches
nothing of value regardless.)

Also fixes a flaky control-plane test (`capacity_is_released_after_builds`):
capacity release is eventual-consistent and lagged the job's terminal transition,
so the test's immediate `vcpus_used == 0` assert raced — now it waits for capacity
to drain first (a real leak still fails, deterministically).

## (pending) — GitOps config sync is server-side only (delete in-browser git)

GitOps config-repo mirroring used to fall back to an in-browser isomorphic-git
path (`gitops-local` → `isogit` → `/api/git/cors-proxy`) whenever the server
route skipped (GitHub not connected), and that browser path was unreliable. The
server-side sync (`/api/gitops/sync` → Composio GitHub API), which is the path
that worked, is now the sole mechanism: `triggerGitopsSync` no longer imports or
runs any browser git — on a skipped sync it records a benign "not configured"
status and the Set-up-GitOps onboarding is how a user connects GitHub. The entire
client-side git subsystem is deleted (`ui/lib/gitops-local.ts`, `ui/lib/isogit.ts`,
`ui/app/api/git/cors-proxy`, and the `isomorphic-git` + `@isomorphic-git/lightning-fs`
dependencies), so the dashboard bundle ships zero browser-git code. Verified
server-only across all fleet nodes (`/api/gitops/sync` 200, `/api/git/cors-proxy`
404, zero isomorphic-git in every bundle).

## (pending) — Fix Redeploy 404 for zip-uploaded and image projects

The Redeploy modal (and `shadw projects redeploy`) returned a bare
`POST /v1/projects/<p>/redeploy -> 404` for any project deployed from a ZIP
upload or a prebuilt image. `project_redeploy` resolved the source through
`git_for_project_fleet`, which filters `GitSource::is_real_git()` and so
returns `None` for the synthetic `upload://`/`image://` pseudo-URLs those
deploys stamp into `repo_url` — a git-only redeploy that never worked for the
non-git half of the platform. Now it resolves the newest source unfiltered
(`source_for_project_fleet`) and rebuilds by kind: git re-clones; images
reconstruct their `image_ref`; zip projects rebuild from RETAINED source —
a durable `<project>.src.zip` kept at upload time (GC-safe, since the build-dir
reaper skips non-directory files), falling back to a copy of the prior build's
on-disk checkout. Because placement stickiness is container-lease-only, a zip
redeploy pins to the node holding the source (local `no_fanout` build, else the
redeploy forwards to the host node over the existing fanout transport). Genuine
failures now return a descriptive 4xx body instead of an empty 404. Verified
live end-to-end on the fleet (the reported `rfc-blog-page` redeploy: 404 → 200,
built to Ready, still serving). Also removed a stray root-level `test.js`
integration harness.

## (pending) — Fix admin incidents page; generic leader→follower store replication for the whole node-local divergence class

The admin incidents page didn't load and "create didn't work": `IncidentStore`
was node-local, so incidents created on the control-plane leader (where every
mutation forwards) were invisible to reads that the dashboard's multi-A DNS
landed on any other node. A fleet audit found this was a whole CLASS —
~12 stores (apikeys, webhooks, databases, domains, integrations, gitops, docs,
notifications, identity, enterprise, teams, incidents) took mutations only on
the leader but served reads from the local store. Rather than hand-write a
gossip arm + adoption block per store, added one generic mechanism
(`store_sync::REGISTRY` + a `/v1/store-snapshot/<name>` gossip arm + a follower
loop that adopts each store's snapshot when it changes). Serialization is
canonicalized through `serde_json::Value` (sorted keys) so the byte-compare
change-gate is stable even for HashMap-backed stores with nested maps — a first
cut re-adopted databases/domains/gitops every tick until that landed.
`apikeys` was the severest case: a key minted on the leader now verifies on
every node instead of failing API auth on followers. Audit replication was
live-verified adopting 659 entries onto a follower. Also added
`DELETE /v1/incidents/:id` (was resolve-only). Secret-bearing stores ride the
existing peer-trust-enforced signed mesh.

Edge-enforcement config (WAF rules, redirects/rewrites, bot policy) replicates
too, so every node enforces the leader's config. Cron is the exception — its
jobs are split across nodes (manual jobs on the leader, `vercel.json` jobs on
the building node), so `cron_list` fans out read-only and merges instead
(execution stays per-node, never double-firing).

`securelinks`/`audit` gained `snapshot()`/`load()`; `Team`/`Member`/`Incident`
gained `PartialEq`; `vendor/guardian-db` is now a workspace-excluded path dep so
its own test suite runs standalone.

## (pending) — fc-sanjose-2 bring-up; fix workflows-page 400, teams-mirror corruption, and guardian-db index flakiness

Brought up the fleet's 10th node, fc-sanjose-2 (43.173.78.95): PVM kernel
built from `virt-pvm/linux@pvm-612` (the ansible `pvm_firecracker` role now
clones that base and applies the fsgsbase/rdtscp fallback patches only on
hosts that need them — the patch repo's own `kernel/` dir is a non-buildable
browsing tree), firecracker-next, podman+gVisor, backend, dashboard, embedded
+ standalone relay, guardian replication, and a hardened lockdown (9090
blocked publicly). `hive-lockdown.sh` is now git-tracked and deployed by the
`prerequisites` role, which also disables+masks `firewalld` — a fresh Rocky
10 image ships it active and its restrictive default zone silently blocked
the relay/discovery ports underneath the hive rules. The PVM role gained a
`pvm_already_provisioned` idempotency gate (a re-run on an already-PVM host
used to self-referentially re-configure from the PVM kernel's own config and
break the build).

`GET /v1/workflows/runs?summary=1` returned 400 (the dashboard workflows page
couldn't load runs): axum's `Query` bool deserialization only accepts literal
`true`/`false`. All 7 `Option<bool>` query fields in `admin.rs` now use a
lenient deserializer accepting `1`/`0` (matching the mesh-RPC path's existing
convention); swept every crate — the class is confined to `admin.rs`.

The relational teams mirror lost 3 of 5 real teams fleet-wide: the TeamStore
itself diverges across nodes (mutations land only on the control-plane
leader; followers never merged them), so a brief failover put a stale
stand-in in charge of `sync_teams`, whose delete-reconciliation wiped every
team it had never heard of. Followers now adopt the leader's TeamStore each
mirror tick via a new `/v1/teams/snapshot` mesh arm, and the delete phase is
tombstone-guarded (`updated_ms` staleness) so a live leader's rows can't be
wiped. Separately, guardian-db's document-store index rebuild dropped rows
whose content-blob fetch transiently failed (table counts flickered to 0 on
healthy nodes) — it now falls back to the previously-fetched value. Every
guardian SQL op is also bounded by a 10s timeout (a corrupted first-open used
to hang reads forever with zero signal).

## 88fe215 — Fix multi-tenancy data-visibility/usage-consistency bugs; add relational layer on guardian-db 0.18; add per-node relay + guardian-db anti-entropy

Team Simpfi's `drugs-wtf` project and fleet-wide billing/metrics were reported
missing/inconsistent — `ProjectStore`/`BillingStore` were node-local-only with
no gossip replication. Adds a relational mirror on GuardianDB's native SQL
layer (upgraded 0.17.2 → 0.18.0) plus a fleet-aware fallback in
`gitops_projects`, closing the visibility gap. Separately root-caused the
actual cross-node read failures to a missing `fetch_from_host`
discovery-fallback and missing `gossip::dispatch` routes for the billing
endpoints — fixed both; billing now converges byte-identical fleet-wide.

Implements the requested per-node relay + guardian-db anti-entropy
architecture: every node embeds and gossips its own relay, a relay-selection
algorithm replaces stale connect hints, and a 60s anti-entropy loop detects
and reconciles guardian-db divergence between random peers — live-verified
converging a real fleet divergence.

Found and fixed a live plaintext-secret leak (a real GitHub PAT served
unmasked via project settings) with a credential-shape auto-detector, and
cleaned up 3 abandoned duplicate Simpfi projects.

## a29c4f1 — Docs + a one-time fleet-wide sweep for existing leaked secrets

Ran the new credential-shape detector as a one-time backfill across every
project on every live node (not just future writes) and found two more real,
already-exposed OpenAI API keys (`fatni`, `shoomoo`) beyond the original
GitHub PAT — resealed all of them; a follow-up re-scan confirmed zero
credential-shaped values remain stored as non-sensitive fleet-wide.

## c2bbfce — Add caching + a read-only PostgreSQL/tables view to the admin Data Browser

The Data Browser's collection/row/table reads are now client-cached for 15s
(mutations still bust the cache immediately, unchanged). Adds a Documents |
Tables (PostgreSQL) view toggle backed by the relational layer above: two
new admin endpoints (`GET /v1/admin/sql/tables`, `POST /v1/admin/sql/query`)
enforced SELECT-only server-side, live-verified end-to-end through the real
dashboard including the guardrail actually being exercised via the UI's own
query box. Along the way, confirmed the dashboard's `/ops` proxy forwards to
the current control-plane leader regardless of which node runs the
dashboard process — a new admin endpoint needs the leader deployed first.

## 41459ad — Fix stale launchd labels in shadw-watchdog.sh

The KeepAlive-backstop watchdog (`dev.shadw.watchdog`) still targeted the
pre-rename labels `dev.shadw.node-a`/`dev.shadw.node-b`, so every 30s tick
silently no-op'd after the local nodes were renamed to
`dev.shadw.fc-lax`/`dev.shadw.fc-lax2` — letting fc-lax2 sit crashed (a
third-party `noq`/`noq-proto` panic-in-destructor abort) for 9h13m despite
launchd `KeepAlive=true`. Updated the watchdog's targets to the current
labels; verified live.

## 166ea99 — Fix stale fleet dashboard + a stale va binary; add scripts/deploy-ui-fleet.sh

The Data Browser's Tables (PostgreSQL) view (c2bbfce) was invisible on the
real https://shadw.cloud because that rollout updated only the backend
binary, never the ui/ dashboard (systemd `hive-ui.service`) — every public
node kept serving a pre-feature build. Separately, va was serving a stale
`hive-cloud` binary that predated the SQL routes entirely (a prior rebuild
was never swapped into its live path). Rebuilt+restarted the dashboard on
all 6 public nodes and swapped in va's correct binary; live-verified via
the compiled bundle, the backend routes, and a real browser hit against
shadw.cloud. Adds `scripts/deploy-ui-fleet.sh` so this doesn't regress
silently again.

## f6c9798 — Add teams/team_members/deployments to the admin SQL view + full billing backfill

The SQL (PostgreSQL) view was missing whole surfaces: no teams, users
(team_members), or deployments tables, and `billing_accounts` only held
tenants actively metered on a tick (4 rows while 20+ tenants existed).
Adds three view-only tables plus `spawn_relational_mirror_loop` — teams,
members, and a FULL billing snapshot backfill sync from the control-plane
leader, own-deployments from every node, content-hash-debounced so quiet
ticks write nothing. Fleet-rolled and live-verified: billing_accounts
4 → 23 rows, real teams/members/deployment rows replicated cross-node,
existing tables and the SELECT-only guard unchanged.

## a2af203 — Watchdog: persistent KeepAlive loop; launchd pended-spawn root cause

fc-lax2 crashed again (same upstream `noq` abort) and sat down because the
watchdog LaunchAgent had never fired: launchd on this long-uptime gui domain
reports `pended nondemand spawn = speculative` and indefinitely defers
StartInterval/RunAtLoad spawns — only demand spawns (`kickstart`) run.
Converted the watchdog to a persistent self-looping KeepAlive daemon
(`WATCHDOG_LOOP=1`), re-bootstrapped, and adversarially verified: SIGABRT'd
fc-lax2 and watched the watchdog restore it autonomously in under a minute.
Separately, fc-virginia and fc-virginia-2 are userspace-frozen (ICMP alive,
all service ports dead from every vantage incl. VPC-internal) and need a
Tencent-console reboot — out of reach from this session.
