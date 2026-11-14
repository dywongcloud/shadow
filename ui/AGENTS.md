# ui/AGENTS.md

The dashboard (Next 16, `ui/proxy.ts` instead of middleware, Clerk, Cache Components): present-tense
conventions, what must be true of the code now; history lives in `CHANGELOG.md`. Cap 14,000 bytes
(`wc -c ui/AGENTS.md`); past it, recompile from the code, the git log and the memory files. Loaded through
`ui/CLAUDE.md` (do not edit it: `next dev` rewrites its block); the root `AGENTS.md` rules still apply.

## Backend calls and sessions

- A Next route or server component calling `backend()` states its auth: `authToken` is required
  (`null` only where anonymous is intended; route handlers pass `authTokenFrom(req)`), because an
  auth-enforced node answers an anonymous GET `200`, empty, `x-hive-anon: 1`. A server prefetch treats that
  header as 401 (`fetchAndValidate`), never as data, and seeds nothing without a token. `/api/git/deploy`
  and `/api/projects/:project/redeploy` forward the platform JWT AND the GitHub clone token as
  `body.git_token` (never returned to the browser; used only for an `https://github.com/` clone).
- Browser side (`ui/lib/api.ts`, `session-clock.ts`, `session-keeper.ts`): `/api/token` echoes the cookie's
  lifetime as `expires_in`, recorded in a clock tabs share (`hive_session_clock`); ONE keeper per tab
  (`startSessionKeeper`) re-mints at ~70%: hidden tabs never mint, a visible one under 20% left mints
  at once, failures follow `MINT_MAX_CONSECUTIVE_FAILURES` then one attempt per period. Mints run one
  at a time (`mintSessionToken`; a team switch/identity sync always fires). A request waits only when no
  session exists or it lapses (`ensureSessionMinted`); a 401 or `x-hive-anon: 1` retries after at most
  one re-mint per `AUTO_MINT_COOLDOWN_MS`, but a 403 on a KNOWN-valid session never re-mints: the
  dashboard is the one `/v1/token` peer of every user. GET-cache keys are tenant-scoped
  (`tenantScopedKey`), any mutation clears the cache, one bypassing `apiSend` calls
  `invalidateApiCache()` itself. `x-hive-anon: 1` 200 is NEVER cached; 2FA challenge is
  DETACHED from `mintInFlight` (no read waits on a human); OTP form is MODULE-scope and
  success fires `hive-otp-satisfied` + cache clear. See `docs/account-2fa.md`.
- The dashboard mirrors the backend role matrix (`crates/hive-cloud/src/roles.rs` `RULES`) in ONE table,
  `ui/lib/team-role.ts` `TEAM_ACTIONS` (action -> floor + covered routes: a `RULES` change is a diff
  against it), read through `can`, `useTeamRole` and `useCan`. The role is the mint answer's `role`
  (mirrored to `hive_role`); `null` = unknown and OFFERS every control, because the server is the authority.
  A control the role cannot use is disabled with a reason (`RoleGate`, or `disabled` + `roleHint`), and a
  403 `insufficient team role` becomes an `ApiError` coded `ROLE_REFUSED` whose message is the sentence:
  show it inline or via `reportError`, never `alert(String(e))`.
- The mint (`ui/app/api/token/route.ts`) derives the tenant SERVER-SIDE from the verified Clerk session:
  an org slug only after membership check, else `u_<uid>`, legacy `personal` only for `HIVE_OWNER_EMAIL`;
  a client-sent `team` is a request, never a grant. The backend trusts it only because the call carries
  `x-hive-internal`, and re-derives `platform_admin` from the forwarded email. `HIVE_OWNER_EMAIL` is read
  server-only via `ui/lib/owner-emails.ts`; `ADMIN_EMAILS` exists TWICE (`ui/proxy.ts` gates `/admin`+`/ops`,
  client `ui/lib/owner.tsx` gates display) and the node's list is a third: edit together. `prior_token`
  comes from the httpOnly cookie, NEVER a client body field; `force_otp` is a stricter-only boolean.
- The `hive_jwt` cookie is httpOnly with the parent-domain `Domain` from `x-forwarded-host` FIRST
  (behind hive-node's dashboard proxy the Next Host is the loopback upstream), so it also rides the
  `wss://api.<domain>` sandbox-shell handshake; a host-only cookie makes that upgrade anonymous (403,
  "WebSocket connection failed"), and the mint expires a legacy variant.
- Every tenant change goes through `switchTeam` (persist, RE-MINT `hive_jwt`, THEN `hive-team-changed`):
  raw `setItem`+dispatch leaves the old account's cookie authenticating pollers with valid 200s. Personal
  namespace is `u_<uid>` (legacy `personal` only when the backend marks the owner); transient `"local"`
  (Clerk loading) never counts as a user change. Whole-page auth flips use `useSettledAuth()`, never raw
  `<SignedIn>/<SignedOut>`.
- `fetchOpsServer` (`ui/lib/ops-data.ts`) has real operator authority (`HIVE_INTERNAL_TOKEN` authorizes
  minting only): call only from pages behind `proxy.ts`'s owner allow-list (`/admin`, `/ops`);
  `fetchOpsResult` returns `{data, error}` so a refused read is never rendered as empty.

## Outbound requests, redirects, secrets

- A request-derived server fetch goes through `safe-fetch.ts` `safeGet`: addresses validated in the
  connect-time `lookup` of `node:https` (`public-address.ts`: one non-public answer refuses; IPv4-mapped/
  NAT64 unwrapped), redirects followed by hand (<= 3, each hop re-validated), bodies size/type-bounded;
  never a pre-check followed by `fetch` (DNS rebinding).
  `/api/tv/hls` stays an anonymous public relay (owner decision): `role=key` is CLIENT-SUPPLIED (relabels
  <= `KEY_MAX_BYTES` of a public body as inert `application/octet-stream`); gzip/deflate is decoded
  (`boundedSource`) under encoded AND decoded caps (br, zstd, chained 415; a 206 in a coding 502).
- A request-derived redirect target/return path passes `ui/lib/same-origin-path.ts` `sameOriginPath`
  (`startsWith("/")` accepts `//evil.example`, `/\evil.example`, `/<TAB>/evil.example`). A host flowing
  into a URL is validated as a bare hostname before any suffix test.
- `ui/app/api/preview-unlock` authorizes only from the node's `/v1/zkauth/project-team` answer plus the
  Clerk identity (`teamAccess`, also behind `/api/zk-enroll`), never `?team=`, failing closed: an
  unanswerable lookup is 503 (`Retry-After`), a non-member 403; `zkauth::bootstrap` binds the ring to the
  owner. Proof URLs come only from a platform-issued host (`isKnownDeploymentHost`), `http` only for
  `localhost`.
- GitHub connect state (`githubConnectionDetail`): only a token GitHub just answered for may report
  `connected` — HELD but unchecked: `connected`+`degraded`; `unreadable`/`foreign` never "not connected".
  `configured` is `githubConfigured()` (App OR Composio) on EVERY branch. `GET /app/installations` spans
  every customer, never evidence for THIS user. The App token lives ENCRYPTED in a host-only httpOnly
  cookie (widening its domain needs the operator's word); `getGithubAppToken()` is Route-Handler-only. OAuth
  `state` signs `returnTo` + the VERIFIED Clerk uid; a 10-min nonce binds it and the callback consumes it. The
  nonce alone is parent-domain scoped (www/apex callback hop), `secure` follows the request proto, and
  `verifiedUserId()` in `lib/session-identity.ts` is the ONE identity resolver for `/api/token`, the credential
  binding, the Composio entity and the OAuth state: bare `auth()` reads signed-out on iOS Safari while `__session`
  is valid. A failed connect returns to `returnTo` with `github_error`; the SW skips `/oauth`. A failed status
  check never opens the onboarding modal or claims "not connected".
- GitOps writes are DEVICE-INDEPENDENT (cookie THEN App installation token), never gated on
  `githubStatus`'s cookie. `/api(.*)` is Clerk-public: EVERY write needs an ASSERTED platform JWT
  (`gitopsCallerProof`; `x-hive-anon: 1` fails closed). `project-ci` takes a caller-chosen repo: ALSO
  admin floor, fail CLOSED on an unknown role. `init` links only AFTER a successful commit; creating a
  repo needs a user grant. `GET /api/github/status` reports the installation state (`login:null`, no
  scopes, `installationRepo`) ONLY when GitHub mints a token for the tenant's linked repo from
  `/v1/gitops` (never its sticky `connected` flag or global list); await that bounded mint FIRST.
  A `none` grant with an unfinished check reads `unknown`. Status consumers show honest copy + a
  device-connect CTA, never a repo picker or Disconnect. `gitops` is WHOLESALE-REPLACE: `load()`
  clears, or unlinks never reach followers.
- Committed GitOps artifacts never carry secrets (`databaseNode` is a field allowlist,
  `projectNode` serializes env KEYS only). `composio.commitFiles`/`github-rest.ghCommitFiles` are ONE
  contract with TWO implementations (an unresolvable branch tree REFUSES: no `base_tree` deletes the
  repo); callers refuse to commit while `buildOrgArtifacts` reports `failures`.

## Data sources and polling

- Every project picker uses `useProjectNames()`/`useProjects()` (deployments + backend inventory),
  never `deployments.map`: a project whose builds all failed stays listed/actionable
  (`hasNoDeployment`: Retry/Delete).
- `usePoll`/`useOpsPoll` share one loop (`usePollWith`): hidden tabs never poll and catch up on
  `visibilitychange`; a new hook binds a fetcher, never copies the body. Fan-out read polls
  (`/v1/functions`, `/v1/metrics`, `/v1/admin/overview`) stay under the 15 s polled-response cache
  TTL (`OVERVIEW_POLL_MS`, `FUNCTIONS_POLL_MS`).
- Dashboard numbers are measured or absent, never invented: counters from `/v1/metrics`/
  `/v1/functions`, prices only from the billing API's `rate_card`; unmeasured renders absent;
  the dashboard rolls BEFORE or WITH the nodes.
- A server/client-differing value (`window.location`, `localStorage`) never seeds a `useState`
  initializer (React #418): pass from the server or read in a post-mount effect.
- The public `/status` board reads `PUBLIC_INCIDENTS_PATH` (`/v1/status/incidents`: operator-origin
  only, automated ones carry node names/pids/command lines); `/v1/incidents` stays operator-gated,
  so never point the page at it. A new server-derived wire field is OPTIONAL in its TS type with an
  exact fallback: UI/backend deploy independently, a fresh UI talks to nodes that predate it.

## Proxy, CSP, caching

- `ui/proxy.ts`: `/ops(.*)` is the privileged admin proxy, sharing `/admin(.*)`'s owner gate
  (`isAdminRoute`); `/wfc(.*)` is public to Clerk (data-plane auth = `hive_jwt`); `isClerkFree` routes skip
  `clerkMiddleware` and are enumerated positively; `HIVE_AUTH_BYPASS=1` only when `NODE_ENV !== "production"`.
  `/cloud/v1/zkauth/*` is blocked at the public rewrite (enroll/proof mint run server-to-server via
  `/api/preview-unlock` and `/api/zk-enroll`); the matcher excludes `.wasm`/`.mjs`.
- Self-hosted `next start`: an `x-middleware-rewrite` to the request's own URL loops over the network
  forever, so `neutralizeClerkSelfRewrite` downgrades it and `auth().protect()` must pass
  `unauthenticatedUrl`; sign-in return comes from `x-forwarded-host`/`-proto`, never `req.url`.
  `corsWrapped` echoes the Origin only for OUR apex/www hosts on `/api/*` and `/cloud/*`, with
  `Vary: Origin`, never `*`.
- `LANDING_CACHE` for `/` stays byte-identical in `proxy.ts` and `next.config.mjs` (config copy LAST so it
  beats the catch-all); settings/deployments/network/billing/admin/`/workflows` are `no-store`. CSP is enforcing:
  `script-src` keeps `'unsafe-inline'`/`'wasm-unsafe-eval'` (run-node), `'unsafe-eval'` only outside
  production, `connect-src` adds `wss://*.shadw.cloud` (Next rewrites never proxy an Upgrade); a LIVE
  Clerk key drops the `*.clerk.accounts.dev` wildcard.

## Browser lane and workflow console

- The run-node SharedWorker is keyed by URL + name and outlives reloads, so both carry `HOST_ABI_VERSION`
  (`run-node-status.ts` = `run-node-worker.js`): bump both, never ask users to close tabs. Neither worker
  has `RTCPeerConnection`/`Worker`, so the PAGE brokers four lanes (`dbWorker`, `meshPeer`,
  `syncFsRegister`, `nodeWorker`); without SharedWorker one tab wins the `shadw-run-node-owner` Web Lock.
  `sync-browser-node.mjs` lists every hand-written `crates/hive-browser/www/` runtime module (a missing
  one 404s only on the fleet); `sw.js`/`sw-handler.js` land under `/browser-node/node-worker/`.
- While the session mint is in backoff, `/deployments` answers a real 200 scoped to an anonymous tenant
  (`[]`): the picker judges a persisted selection only when `sessionMintStatus().state` is `"ok"` or
  `"unattempted"`.
- Workflow console embed: `patch-wf-console.mjs` applies idempotent sentinel-guarded patches to the
  compiled `@workflow/web` bundles (anchors target the PRISTINE package: `no-anchor` means reinstall fresh);
  `hive-world.mjs` is its `WORKFLOW_TARGET_WORLD` (`HIVE_WF_TIMEOUT_MS` must exceed `/v1/workflows/runs`
  budgets); the iframe mounts only after the first `hive_jwt` mint. `[[...slug]]/route.ts` keeps
  `import "@workflow/web/server"` so Next traces the package.

## Build, deploy, witness

- `ui/` ships separately from the backend (`scripts/deploy-ui-fleet.sh`, `ansible/AGENTS.md`); verify
  on the real domain. GitOps CI posts to `/v1/git/webhook` with `x-hub-signature-256: sha256=<hex>` =
  HMAC-SHA256 of the exact payload keyed by the node's `GITHUB_WEBHOOK_SECRET`;
  `/api/gitops/project-ci` installs a real repo webhook first and the Actions workflow ONLY when the
  webhook could not be installed, never both.
- Witness a dashboard change as `http://localhost:PORT`, never `127.0.0.1` (Next 16 never hydrates
  cross-origin dev assets). Local dev without Clerk: `HIVE_AUTH_BYPASS=1` +
  `NEXT_PUBLIC_HIVE_DEV_MINT=1` mints a LOCAL JWT so the real mint -> cookie -> tenant path runs. `next dev`
  rewrites `ui/next-env.d.ts` (`git_checkout paths:["ui/next-env.d.ts"]`, never commit it) and can miss
  rapid route-handler edits (restart before measuring); time client behaviour in a production build.
  The sandbox-shell WebSocket needs a raw stdlib client (WHATWG cannot send `Authorization`): witness a
  NON-owner node against the owner.
- Checks run from `ui/`: `npx tsc --noEmit`, `npm run lint` (exclude `.next/dev/types`). Never dispatch
  gm from `ui/` as the cwd: it hits `ui/.gm`, a separate graph.
