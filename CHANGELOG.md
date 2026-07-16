# Changelog

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
