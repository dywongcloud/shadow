# ui/AGENTS.md

The dashboard (Next 16, `ui/proxy.ts` instead of middleware, Clerk, Cache Components): present-tense
conventions, what must be true of the code now; history lives in `CHANGELOG.md`. Cap 14,000 bytes
(`wc -c ui/AGENTS.md`); past it, recompile from the code, the git log and the memory files. Loaded through
`ui/CLAUDE.md` (do not edit it: `next dev` rewrites its block); the root `AGENTS.md` rules still apply.

## Backend calls and sessions

- A Next route or server component that calls `backend()` states its auth: `authToken` is a required
  parameter (`null` only where anonymous is intended; route handlers pass `authTokenFrom(req)`), because an
  auth-enforced node answers an anonymous GET `200`, empty, `x-hive-anon: 1`. A server prefetch treats that
  header as 401 (`fetchAndValidate`), never as data, and seeds nothing without a token. `/api/git/deploy`
  and `/api/projects/:project/redeploy` forward the platform JWT AND the GitHub clone token as
  `body.git_token` (looked up within `GITHUB_TOKEN_LOOKUP_DEADLINE_MS`, never returned to the browser; the
  backend uses it only for an `https://github.com/` clone).
- Browser side (`ui/lib/api.ts`): every `/cloud` + `/ops` call awaits ONE shared session mint
  (`ensureSessionMinted`) and treats 401, 403 AND `x-hive-anon: 1` as an invalid session: one re-mint +
  retry, at most once per `AUTO_MINT_COOLDOWN_MS` (30 s); automatic mints share a failure budget
  (`MINT_MAX_CONSECUTIVE_FAILURES` 4, then terminal `failed`) so a browser that can never persist the cookie
  stops POSTing `/api/token`; explicit mints (team switch, identity sync) always fire. GET-cache keys are
  tenant-scoped (`tenantScopedKey`) and any mutation clears the cache; a mutation that bypasses `apiSend`
  (the zip upload) must call `invalidateApiCache()` itself.
- The mint (`ui/app/api/token/route.ts`) derives the tenant SERVER-SIDE from the verified Clerk session: an
  org slug only after a membership check, else `u_<uid>`, and the legacy `personal` only for a
  `HIVE_OWNER_EMAIL` account; a client-sent `team` is a request, never a grant. The backend trusts it only
  because the call carries `x-hive-internal`, and re-derives `platform_admin` itself from the forwarded
  email. `HIVE_OWNER_EMAIL` is read on the server only through `ui/lib/owner-emails.ts`; `ADMIN_EMAILS`
  exists TWICE (`ui/proxy.ts` enforces `/admin` + `/ops`, client-bundled `ui/lib/owner.tsx` gates display)
  and the node's `platform_admin` list is a third: edit them together. Client owner gating (`hive_is_owner`,
  `usePlatformAdmin()`) only decides what the UI SHOWS.
- The `hive_jwt` cookie is httpOnly with the parent-domain `Domain` taken from `x-forwarded-host` FIRST
  (behind hive-node's dashboard proxy the Next server's Host is the loopback upstream), so it also rides the
  `wss://api.<domain>` sandbox-shell handshake; a host-only cookie makes that upgrade anonymous (403,
  "WebSocket connection failed"), and the mint expires a legacy host-only variant. Diagnose with
  `GET /v1/admin/audit` first: a refused upgrade leaves no handler log line.
- Every tenant change goes through `switchTeam` (persist, RE-MINT `hive_jwt`, THEN `hive-team-changed`): a
  raw `setItem` + dispatch leaves the previous account's cookie authenticating pollers as the OLD tenant
  with valid 200s. The personal namespace is `u_<uid>` (legacy `personal` only when the backend marks the
  owner); the transient `"local"` identity (Clerk still loading) never counts as a user change. Whole-page
  auth flips render from `useSettledAuth()`, never raw `<SignedIn>/<SignedOut>`.
- `fetchOpsServer` (`ui/lib/ops-data.ts`) runs with real operator authority (it exchanges
  `HIVE_INTERNAL_TOKEN`, which authorizes minting only): call it only from pages behind `proxy.ts`'s owner
  allow-list (`/admin`, `/ops`); `fetchOpsResult` returns `{data, error}` so a refused read is never
  rendered as an empty one.

## Outbound requests, redirects, secrets

- A server fetch whose URL derives from a request goes through `ui/lib/safe-fetch.ts` `safeGet`: addresses
  are validated in the connect-time `lookup` of `node:https` (`ui/lib/public-address.ts`: one non-public
  answer refuses the target; IPv4-mapped/NAT64 unwrapped), redirects are followed by hand (<= 3, every hop
  re-validated), bodies are size- and type-bounded; never a pre-check followed by `fetch` (DNS rebinding).
  `/api/tv/hls` stays an anonymous public relay (owner decision), so its bounds are its only protection:
  `role=key` is CLIENT-SUPPLIED and no authorization (it only relabels at most `KEY_MAX_BYTES` of a public
  body as inert `application/octet-stream`), and gzip/deflate bodies are decoded (`boundedSource`) under
  caps on the encoded AND decoded bytes (br, zstd and chained codings 415, a 206 in a coding 502).
- A request-derived redirect target or return path passes `ui/lib/same-origin-path.ts` `sameOriginPath`
  (`startsWith("/")` accepts `//evil.example`, `/\evil.example`, `/<TAB>/evil.example`). A host that flows
  into a URL is validated as a bare hostname before any suffix test.
- `ui/app/api/preview-unlock` authorizes only from the node's `/v1/zkauth/project-team` answer plus the
  Clerk identity (`teamAccess` in `ui/lib/preview-access.ts`, also behind `/api/zk-enroll`), never `?team=`
  (it travels through the visitor's browser), failing closed: a node or Clerk lookup that cannot answer is a
  retryable 503 (`Retry-After`; the unlock page says "unavailable", not "denied"), a non-member 403;
  `zkauth::bootstrap` independently binds the ring to the project's owner. The proof URL is built only for a
  platform-issued host (`isKnownDeploymentHost`), `http` only for `localhost`.
- GitHub connect state (`githubConnectionDetail`): only a token GitHub itself just answered for may report
  `connected`; the GLOBAL `GET /app/installations` list spans every customer and is never evidence for THIS
  browser's user. The App user token lives ENCRYPTED in an httpOnly cookie and `getGithubAppToken()` is
  called ONLY from a Route Handler (GitHub rotates refresh tokens single-use: a refresh from a Server
  Component burns the token). The OAuth `state` signs `returnTo` + the Clerk uid and a 10-min nonce cookie
  binds it; the callback always consumes the nonce.
- Committed GitOps artifacts never carry secrets (`databaseNode` is an explicit field allowlist,
  `projectNode` serializes env KEYS only). `composio.commitFiles` and `github-rest.ghCommitFiles` are ONE
  contract with TWO implementations (atomic Git Data commit; `managedPrefixes` tombstones stale paths; a
  branch whose tree cannot be resolved REFUSES: a tree without `base_tree` would delete the repo); callers
  refuse to commit while `buildOrgArtifacts` reports `failures`.

## Data sources and polling

- Every project picker uses `useProjectNames()`/`useProjects()` (`ui/lib/projects.ts`: the deployment list
  merged with the backend inventory `SUMMARY_PATH`), never `deployments.map(d => d.project)`: a project
  whose builds all failed has no deployment yet stays listed and actionable (`hasNoDeployment`:
  Retry/Delete); a failed build's page links to the PROJECT page, never the new-project form.
- `usePoll` and `useOpsPoll` share one loop (`usePollWith`): a hidden tab never polls and catches up on
  `visibilitychange`; a new polling hook binds a fetcher, never copies the body. Polls of the fan-out reads
  (`/v1/functions`, `/v1/metrics`, `/v1/admin/overview`) stay at or under the backend's 15 s polled-response
  cache TTL (`OVERVIEW_POLL_MS`, `FUNCTIONS_POLL_MS`).
- Dashboard numbers are measured or absent, never invented: tenant counters come from `/v1/metrics` and
  `/v1/functions` (a non-operator's `/v1/overview` is topology only), usage prices only from the billing
  API's `rate_card`, and a value the node did not measure renders as absent (the node serves
  `DomainRecord`'s `registrar`/`renewal_price`/`auto_renew`/`expires_ms` EMPTIED for older nodes:
  `ui/lib/api.ts` declares none, and the dashboard rolls BEFORE or WITH the nodes). The cron row label
  (`ui/lib/cron-schedule.ts`) states only schedules it describes exactly.
- A value that differs between server and client render (`window.location`, `localStorage`) never seeds a
  `useState` initializer (React #418): pass it from the server or read it in a post-mount effect.
- The public `/status` board reads `PUBLIC_INCIDENTS_PATH` (`/v1/status/incidents`:
  `incidents::public_board`, operator-origin incidents only, because automated ones carry node names, pids
  and command lines); `/v1/incidents` stays operator-gated, so never point the page at it.
- A new server-derived wire field is OPTIONAL in its TS type with an exact fallback: the dashboard and the
  backend deploy independently, so a fresh UI talks to nodes that predate it.

## Proxy, CSP, caching

- `ui/proxy.ts`: `/ops(.*)` is the privileged admin backend proxy and shares `/admin(.*)`'s owner gate
  (`isAdminRoute`); `/wfc(.*)` is public to Clerk (a 307 to /sign-in feeds sign-in HTML to the console's
  CBOR decoder; its data-plane auth is the `hive_jwt` cookie); `isClerkFree` routes skip `clerkMiddleware`
  and are enumerated positively; `HIVE_AUTH_BYPASS=1` is honored only when `NODE_ENV !== "production"`.
  `/cloud/v1/zkauth/*` is blocked at the public rewrite (enroll and proof mint run server-to-server through
  `/api/preview-unlock` and `/api/zk-enroll`); the matcher keeps excluding `.wasm`/`.mjs`.
- Self-hosted `next start`: an `x-middleware-rewrite` to the request's own URL loops over the network
  forever, so `neutralizeClerkSelfRewrite` downgrades it and `auth().protect()` must pass
  `unauthenticatedUrl`; the sign-in return URL comes from `x-forwarded-host`/`-proto`, never `req.url`.
  `corsWrapped` echoes the Origin only for OUR apex/www hosts, on `/api/*` and `/cloud/*`, with
  `Vary: Origin`, never `*`.
- `LANDING_CACHE` for `/` stays byte-identical in `ui/proxy.ts` and `ui/next.config.mjs` (the config copy is
  listed LAST so it beats the catch-all); settings/deployments/network/billing/admin/`/workflows` are
  `no-store`. The CSP is enforcing: `script-src` keeps `'unsafe-inline'` (no nonce pipeline) and
  `'wasm-unsafe-eval'` (run-node), `'unsafe-eval'` only outside production, `connect-src` adds
  `wss://*.shadw.cloud` (Next rewrites never proxy a WebSocket Upgrade), a LIVE Clerk key drops the
  `*.clerk.accounts.dev` wildcard, and `Permissions-Policy` keeps `geolocation=(self)`.

## Browser lane and workflow console

- The run-node SharedWorker is keyed by URL + name and outlives reloads, so both carry `HOST_ABI_VERSION`
  (`ui/lib/run-node-status.ts`, equal to the one in `ui/public/run-node-worker.js`): ship a worker change by
  bumping both, never by asking users to close tabs. Neither worker context has `RTCPeerConnection`, a
  service worker or a `Worker` constructor, so the PAGE brokers four lanes (`dbWorker`, `meshPeer`,
  `syncFsRegister`, `nodeWorker`); without SharedWorker exactly one tab wins the `shadw-run-node-owner` Web
  Lock. Presence is published from the `useRunNode` hook (mounted app-wide) every `PRESENCE_REFRESH_MS` 45 s
  against the server's 90 s TTL. `ui/scripts/sync-browser-node.mjs` must list every hand-written
  `crates/hive-browser/www/` module the worker imports at RUNTIME (a missing one fails no build and 404s
  only on the fleet); `sw.js`/`sw-handler.js` land under `/browser-node/node-worker/`.
- While the session mint is in backoff, `/deployments` answers a real 200 scoped to an anonymous tenant
  (`[]`): the picker judges a persisted selection only when `sessionMintStatus().state` is `"ok"` or
  `"unattempted"`.
- Workflow console embed: `ui/scripts/patch-wf-console.mjs` applies idempotent sentinel-guarded patches to
  the compiled `@workflow/web` bundles (anchors target the PRISTINE package: `no-anchor` means reinstall it
  fresh); `ui/lib/wf-console/hive-world.mjs` is its `WORKFLOW_TARGET_WORLD` (`HIVE_WF_TIMEOUT_MS` must
  exceed the backend's summed `/v1/workflows/runs` budgets); the iframe mounts only after the first
  `hive_jwt` mint (a cookie-less first rpc settles the runs table on a false "No workflow runs found") and
  re-mints every `REMINT_INTERVAL_MS`. `ui/app/wf-console/[[...slug]]/route.ts` keeps
  `import "@workflow/web/server"` so Next traces the package: never delete it as unused.

## Build, deploy, witness

- `ui/` ships separately from the backend (`scripts/deploy-ui-fleet.sh`, procedure in `ansible/AGENTS.md`);
  verify on the real domain. The GitOps CI workflow posts to `/v1/git/webhook` with
  `x-hub-signature-256: sha256=<hex>` = HMAC-SHA256 of the exact payload keyed by the node's
  `GITHUB_WEBHOOK_SECRET`; `/api/gitops/project-ci` installs a real repo webhook first and the Actions
  workflow ONLY when the webhook could not be installed, never both.
- Witness a dashboard change as `http://localhost:PORT`, never `127.0.0.1` (Next 16 blocks cross-origin dev
  assets: the page renders, React never hydrates). Local dev without Clerk: `HIVE_AUTH_BYPASS=1` plus
  `NEXT_PUBLIC_HIVE_DEV_MINT=1` mints a LOCAL JWT so the real mint -> cookie -> tenant path runs. `next dev`
  rewrites `ui/next-env.d.ts` (`git_checkout paths:["ui/next-env.d.ts"]`, never commit it) and can miss
  rapid edits to a route handler (restart it before measuring); time client behaviour in a production build,
  because strict mode runs mount effects twice in dev. Witness an SSRF or redirect fix with recording
  listeners behind a `cloudflared` quick tunnel. The sandbox shell
  (`GET /v1/projects/:project/sandboxes/:sandbox_id/shell`, a WebSocket on the admin port) needs a raw
  stdlib client to witness (WHATWG `WebSocket` cannot send `Authorization`): hit a NON-owner node and
  compare it with the owner.
- Checks run from `ui/`: `npx tsc --noEmit` and `npm run lint`. Never dispatch gm from `ui/` as the cwd: it
  hits `ui/.gm`, a separate graph.
