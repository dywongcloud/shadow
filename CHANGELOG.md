# Changelog

## 2026-10-13 — HIVE_OWNER_EMAIL is parsed in one place on the dashboard server

The dashboard server parsed `HIVE_OWNER_EMAIL` (a comma-separated list) at four sites with
two semantics: a lowercased, trimmed set with empty entries dropped in `/api/token`'s owner
check and in the preview unlock route's personal-namespace check, and the raw text before
the first comma in `/api/token`'s dev mint, `lib/ops-data.ts` and the integrations catalog
route (found by the ui comment sweep). One server-only helper, `lib/owner-emails.ts`
(`ownerEmails`, `isOwnerEmail`, `primaryOwnerEmail`), parses it exactly like `/api/token`'s
set and all four sites use it. Witnessed over 13 `HIVE_OWNER_EMAIL` values (unset, empty,
one address, upper case, lists with spaces and empty entries): the set semantics are
identical for all 13, and the single address sent in a mint claim is identical for every
value but five, three of which differ only by case (now lower case; the node compares the
claim with `eq_ignore_ascii_case`, and a dev mint against a node with
`HIVE_OWNER_EMAIL="Mixed.Case@Example.COM, second@example.com"` is `platform_admin: true`
before and after) and two where the first entry is empty (`",a@x.y"`), which now yields
`a@x.y`, the address the owner check already accepts, instead of no claim. `lib/owner.tsx`
(the client-bundled operator list) and `proxy.ts`'s own list are untouched. Not rolled.

## 2026-10-13 — the logs page resets its page with the filter, not a render later

`project-logs-client.tsx` reset the page to 0 in an effect keyed on every filter, which is
the one error `eslint` still reported for the dashboard and also a real defect: the render
that changed a filter still carried the old page, so the fetch effect asked the node for
the new filter at the old offset first and for page 0 one render after. The reset is now
derived while rendering (the filter signature is kept in state and compared, the officially
recommended replacement for an effect that only resets state), so there is one correct
request per change and `npx eslint .` is clean. Witnessed in headless Chrome on the logs
page of a throwaway node (page size 100, paged to the second page, then the timeline
switched to one hour): the change triggered a request at `offset=100` and then one with no
offset before, only the offset-less one after; `npx eslint .` exits 0. Not rolled.

## 2026-10-13 — node-to-node RPC routes and the legacy sandbox no longer accept any tenant token

The class scan over every non-GET route behind `auth::require_auth` (which only proves "some
valid token") found four mutation handlers that took no claims at all. `POST /v1/nodes/announce`
let any tenant write a registry entry for any node name (public address, health, peer id, the
fencing epoch): witnessed on a throwaway auth-enforced node, one forged `evil-node` announce
from a tenant token put it in the operator's registry and the node's next mutation (a second
tenant's env write) answered 503, because the forged owner became the control-plane leader this
node forwards its mutating API to. `PUT /v1/push/sms-relay` and `PUT /v1/push/sms-direct-mx`
ran the platform's Textbelt egress and a direct-MX blast for any tenant (the relays exist for the
mesh: their only callers are the gossip arms and `put_to_host`). All three now sit behind
`admin::require_mesh_rpc` (a platform operator, the `x-hive-internal` secret, or a `mesh-internal`
delegation, which is what `gossip::fetch`'s HTTP fallback sends), announce through
`node_announce_public` so the gossip arm keeps calling the unguarded inner behind the transport's
signer and trust gate. `POST /v1/sandbox` (the dashboard's single-use "Terminal" cell) is a real
tenant feature, so it stays open to signed-in tenants but is bound to one: the image name
selects a rootfs and the data drive named like it on the node (a tenant could name another
tenant's deployment image), so naming an image other than `default` is now operator-only (403),
`timeout_secs` is clamped to 5..=120 s (`u64::MAX` panicked the handler on the overflowing
multiply; it is a normal run now), a tenant holds at most 2 cells per node (a Drop-released
slot; the third answers 429), and every run is audit-recorded under the tenant. Witnessed before
and after on throwaway nodes: sms-relay and sms-direct-mx answered 200 to a tenant token before
and answer 403 after, while a mesh-internal delegation and the operator still reach them; the
sandbox accepted `image=dpl-another-tenants-deployment` (200), ran three concurrent cells
(200/200/200) and dropped the connection on `timeout_secs=18446744073709551615` before, and
answers 403, 200/200/429 and 200 after, the plain dashboard run unchanged (200, exit 0, logs
returned). Operator note: nothing to migrate; automation that selected a custom sandbox image
with a tenant token must use an operator credential, and a node's own announce and relay
traffic over iroh is untouched. Not rolled.

## 2026-10-13 — the operator pages stop polling in a background tab

`useOpsPoll` (`lib/api.ts`, behind every `/admin` page) was a hand copy of `usePoll` minus
two behaviours that make `usePoll` well behaved: it ticked on its interval whatever the
tab's visibility, so a background admin tab kept polling every endpoint for as long as it
stayed open, and it had no visibility catch-up, so returning to the tab showed data up to
one interval old (found by the ui comment sweep; its own doc claimed "identical semantics
to usePoll"). The loop now exists once, `usePollWith(fetcher, …)`, and `usePoll` and
`useOpsPoll` are one-line bindings of `apiGet` and `opsGet`, so they cannot drift again.
Witnessed in headless Chrome (the page's `document.visibilityState` toggled exactly as the
hook reads it) counting the requests of `/admin/audit` (4 s interval): visible for 9 s 3
requests before and after; hidden for 14 s 3 requests before and 0 after; and after
becoming visible the first request came 1262 ms later before (the next tick) and 1 ms
later after; the `usePoll` control on `/deployments` is unchanged (3, 0, 2 ms). Not rolled.

## 2026-10-13 — the DNS-only domain page and the preview unlock page no longer show invented data

A DNS-only domain's detail page (`app/domains/[domain]/domain-detail-page-client.tsx`)
rendered the node's creation defaults as if they were facts: an expiration date one year
after the record was first seen, a "$13 per year" renewal price, "OpenEdge" as registrar,
an auto-renewal toggle that renews nothing, an SSL certificate row built from the pending
placeholder (empty id, "Dec 31, 1969", "497761h ago", "Provided free by OpenEdge (Let's
Encrypt)" and a "Renew now" button) and "WHOIS privacy is enabled — your contact details
are protected" with a "Manage WHOIS Privacy" button, none of which has a source (the
platform is no registrar, and a certificate exists only for an attached, verified domain).
The preview unlock interstitial (`app/preview-unlock/page.tsx`) played a scripted
zero-knowledge transcript (random `commit`, `proof` and `nullifier` hex, "verifier: checking
ring signature over team roster", "verifier: constraints 1/1 satisfied") for a fixed 2.2 s
whatever the server said, so a denied visitor watched a verification succeed and then read
"Access denied" (both found by the ui comment sweep, against the rule that numbers are
measured or absent). Now every invented value reads "Not available for DNS-only domains"
with the layout and the real fields (age, CDN, DNS records, nameservers) untouched, the
dead Renew, Renew now, Manage WHOIS Privacy and auto-renew controls are gone, and the
interstitial shows "Verifying membership…" until `/api/preview-unlock` answers, then the
real result at once (the artificial 2.2 s floor is gone with the transcript). Witnessed in
headless Chrome: the DNS-only page before and after (DOM dump and screenshot), and the
interstitial in a production build of the old and the new page against a stub answering
immediately, after 3 s or with a grant: before, a denied visitor saw "$ zkauth: opening
membership session", random hex values and "« verifier: checking ring signature over team
roster" for 2.2 s and then "Access denied"; after, "Access denied" at once, "Verifying
membership…" for the whole 3 s of a slow answer, and "Membership verified — access
granted" followed by the redirect for a grant. Not rolled. Still invented on the node, not
in the dashboard: `default_domain` seeds `registrar`, `renewal_price`, `auto_renew` and a
one-year `expires_ms` (the Expiration Date of an attached domain still shows that last one).

## 2026-10-13 — a tenant token can no longer register or remove another tenant's database replica

`POST /v1/databases/replica` (the primary's cross-region replica fanout) took only the request
body: `auth::require_auth` proves "some valid token" for a mutation, so any signed-in tenant
could `register` a replica whose `connection` fed `apply_db_egress` (attacker-chosen
`BLOB_*`/`DATABASE_URL` values written into a victim project's runtime env; a Postgres/Redis
`kind` also starts a container on the node), `remove` with an empty or unknown `db.id` to strip
the victim's DB env keys, or, knowing a database id and the owner's team string, remove the
record and its container. The only guard compared the stored team with the attacker-supplied
`db.team`. The HTTP route now mounts `database_replica_public`, which runs the new
`admin::require_mesh_rpc` first: a platform operator, the fleet's `x-hive-internal` secret, or a
`mesh-internal` service delegation minted for the replicated database's own team
(`auth::is_mesh_internal`, the one predicate for "a fleet node, not a tenant"). The inner
`database_replica`, which the iroh gossip arm still calls behind the transport's signer and
trust gate, also refuses a `db.project` the claimed team does not own, and the HTTP leg of
`send_replica_rpc` now presents a `mesh-internal` bearer minted for the database's team (it
sent no credential before, so on an auth-enforced node that leg was a 401). Witnessed on
throwaway auth-enforced nodes (tenants `ta` and `tb`): before, `ta` overwrote `tb`'s
`BLOB_BUCKET` with `ATTACKER-BUCKET`, deleted `BLOB_BUCKET`/`BLOB_ENDPOINT`, and deleted `tb`'s
database by id; after, all three answer 403 and `tb`'s env and database are untouched, the
operator, the `x-hive-internal` secret and a delegation minted for `tb` still register `tb`'s
replica, and a delegation minted for `tb` is refused for team `ta`. On two real nodes meshed
over loopback iroh QUIC (signed gossip) and over `--peer` admin URLs, a database created with
replicas in both regions still lands its copy on the other node, before and after. Operator
note: nothing to migrate; anything other than the platform's own fanout that called this route
with an ordinary tenant token now gets 403. The admin.rs half of this change landed inside the
cron-purge commit `350d417d34` (a path-scoped commit of the shared file took the whole diff);
this commit carries the `auth.rs`/`db_replicate.rs` half and the rustfmt of two lines. Not rolled.

## 2026-10-13 — the cron page no longer labels a seconds-level or ranged schedule as something else

`describeSchedule` (the English label on a cron job row) dropped the first field of every
6-field expression whatever its value, so `*/5 * * * * *` (every 5 seconds) and
`30 * * * * *` were labelled "Every minute", against its own promise never to lie about a
schedule it cannot confidently describe (found by the ui comment sweep). It now lives in
`lib/cron-schedule.ts` and drops the leading seconds field only when it is exactly `0`; any
other seconds field returns null and the row shows the raw expression. The same table
turned up other shapes it mislabelled, which now fall back to the raw expression too: a
step that does not divide its field (`*/7`, `*/90`, `*/0`, `0 */5 * * *` fires at uneven
gaps), lists and ranges in the minute or hour (`0 9-17 * * *` was "Daily at 9-17:00 UTC",
`0,30 9 * * *` "Daily at 09:0,30 UTC") and out-of-range numbers (`60 9 * * *` "Daily at
09:60"). Witnessed by running the real function over 44 expressions: 18 were mislabelled
before (the three seconds-field cases among them) and none are after; and in the browser
against a throwaway node with four jobs, the page showed `*/5 * * * * *` as "Every minute"
(the job had run 16 times in 80 s) and `0 9-17 * * *` as "Daily at 9-17:00 UTC" before, and
shows the raw expression alone for both after, with `0 */5 * * * *` ("Every 5 minutes") and
`30 9 * * *` ("Daily at 09:30 UTC") labelled as before. Not rolled.

## 2026-10-13 — every project picker lists the projects whose builds all failed

The Drive page, the Storage page (`useDatabases`, which feeds the SQLite create form and
the SQLite detail page), the webhook project filter, the Domains wizard, the domain
detail's Connect dialog and the TV recorder's destination derived their project list from
`/deployments` alone, so a project whose first build failed (no deployment, name still
reserved, listed by the dashboard and the command bar through `lib/projects.ts`) could not
be chosen anywhere else (found by the ui comment sweep: Drive and Storage; the other four
share the same `Array.from(new Set(deployments.map(d => d.project)))`). One hook now backs
every picker, `useProjectNames` (`lib/projects.ts`), the alphabetical names of the same
merged list `useProjects` gives the dashboard, and all six sites use it; the Domains page
keeps its managed-domain rows on deployed projects only, since an undeployed project has no
live domain. Witnessed in headless Chrome against the failed-first-build harness (`uiok`
deployed, `uifail` with a missing root directory and `uifailcmd` with a failing build
command, neither with a deployment): before, the Drive select, the Storage SQLite project
select and the webhook project chips offered only `uiok`; after, all three offer `uifail`,
`uifailcmd` and `uiok`, the Domains wizard select the same three while its managed rows
still list only `uiok`, and Drive with `uifail` selected shows its empty folder without an
error. Not rolled.

## 2026-10-13 — the dashboard's server-side backend calls can no longer forget the caller's session

`backend()` (`lib/gitops-server.ts`) took its `authToken` as a trailing optional argument,
so a call site that forgot it silently talked to the node as the anonymous tenant, and an
auth-enforced node answers that with `200`, an empty body and `x-hive-anon: 1`: the
integration link route (`app/api/integrations/link/route.ts`) read `/v1/gitops/projects`
that way, so env auto-injection never found a project and `injected` was always 0, and the
observability page prefetch (`lib/observability-data.ts`, `app/observability/page.tsx`)
seeded its first paint with the anonymous summary, zeros for every tenant, until the first
live poll replaced it (both found by the ui comment sweep). The class is closed at the
primitive: `backend(path, team, authToken, init?)` now REQUIRES the token (an explicit
`null` only where an anonymous read is intended), `buildOrgArtifacts(team, authToken,
rootPath?)` and the observability fetchers require it too, so `tsc` rejects every call that
omits it (witnessed with a probe file: `backend(path, team)`, `backend(path, team, {init})`,
`buildOrgArtifacts(team)` and `fetchMetricsSummary(team, 5)` all fail to compile, the
explicit `null` compiles) and all 21 `backend()` call sites and the four observability
fetch calls were fixed. The observability prefetch now
forwards the session cookie (`sessionAuthToken()`, the `hive_jwt` the ops console already
reads) and, with no token, seeds nothing, so the client renders its loading state until the
first live poll instead of zeros; `fetchAndValidate` treats an `x-hive-anon` answer as a
401, so a rejected session is never rendered as an empty summary, and the two dead
observability route handlers answer 401 without a token instead of anonymous zeros. The
link route reports `injectionError` when the projects read fails instead of swallowing
it. Witnessed on a throwaway auth-enforced node with a tenant token and 16 real requests
on its project: the link route (real handler, Composio replaced by a stub) returned
`injected: 0` before and `injected: 1` after, and the node then lists the injected variable;
the `/observability` first paint seeded `{"projects":[],"series":[{…zeros}]}` for a valid
session cookie before and `{"projects":[{"project":"victimproj","requests":16}]…` after,
with no cookie or a garbage one `null` (loading state). Not rolled.

## 2026-10-13 — the preview unlock route authorizes only from the node's answer and fails closed

`GET /api/preview-unlock` (the dashboard half of the protected-preview unlock: Clerk says
the signed-in user belongs to the deployment's team, then the route enrolls them on the
node and mints the membership proof) fell back to the client-supplied `?team=` whenever
the node's `project-team` lookup threw, answered non-OK (other than 403), answered no
team or `__untagged__`, or returned a body that did not parse; the membership check then
ran against the caller's OWN org, and the node's `/_shadw/zk` verifies a proof against
whichever team ring `t` names without tying it to the project's owner, so that proof
unlocked any project name. The legacy `personal` check let everyone in when
`HIVE_OWNER_EMAIL` was unset and when the Clerk lookup threw, and the proof URL used
plain `http` for any host merely containing `localhost`. Found while reading the call
chain: `isKnownDeploymentHost` did a suffix test on the raw string, so `host=evil.example/
x.localhost` and `evil.example#.localhost` passed and the route minted a proof for the
signed-in member and handed it to an arbitrary origin, and `next` reached the node
unfiltered (which accepts `/\evil.example`). Now the team is ONLY the node's answer (an
unanswerable lookup is a retryable 503, a non-OK one a 502, 403 keeps its 502, an empty or
untagged answer is the legacy `personal` namespace), the personal and org checks share a
`granted | denied | unavailable` result so a Clerk failure is a retryable 503 instead of a
verdict, no configured owner is permissive only outside `NODE_ENV=production`, the scheme
is `http` only for `localhost` and `*.localhost`, the host must be a bare hostname, `next`
goes through `sameOriginPath`, and node calls have a 10 s timeout. Witnessed by running
the real route handler against a throwaway auth-enforced node (`HIVE_INTERNAL_TOKEN` set)
with Clerk replaced by an identity stub and a fault-injecting proxy in front of ONLY
`project-team`: a member of `attacker-org` asking for `?team=attacker-org&project=
victimproj` was ACCEPTED before for a 500, a 404, a dropped connection, an empty team and
an untagged project, and that proof verified on the node's gateway for victimproj's host
(`302` plus a `shadw_zk` cookie); after, those five are 503, 502, 503, 403 and 403 and no
proof is minted. An owner and a real team member still unlock, development without an
owner still lets anyone in, production without one now refuses, a throwing Clerk lookup
is 503 (it was accepted), `evil-localhost.shadw.app` gets https (it got http) and the
smuggled hosts are 400. Not rolled. Still open in hive-cloud: `zkauth::bootstrap` accepts
a proof from any team ring for any project, so the dashboard route is the only place the
project's owning team is enforced.

## 2026-10-13 — the GitHub OAuth return path can no longer leave the site

The connect route (`app/api/github/connect/route.ts`), `verifyState` (`lib/github-app.ts`)
and the OAuth callback accepted any return path that began with `/`, which includes the
protocol-relative `//evil.example` and `/\evil.example` (browsers read the backslash as a
slash); the callback then resolved `new URL(`${returnTo}…`, origin)` to the external host,
so a signed state carrying such a path redirected the user off-site after the OAuth
round trip (found by the ui comment sweep; it needs a validly signed state plus the
victim's nonce cookie, so it was low severity, but the guard was weaker than its stated
intent). One shared helper, `sameOriginPath` (`lib/same-origin-path.ts`), now accepts a
path only when it starts with a single `/` followed by neither `/` nor `\`, carries no
control character (URL parsers strip tab and newline, which turns `/<TAB>/evil.example`
into `//evil.example`) and still resolves to the same origin. It is used where the path
enters (the connect route, fallback `/new`), where it is read back out of the signed
state (`verifyState`, fallback `/`) and again in the callback before it redirects
(fallback `/`). Witnessed through the real `github-app` module and `next dev` with a
GitHub App test configuration: of 37 return paths (23 hostile: `//evil.example`,
`/\evil.example`, `///`, tab, newline, CR, NUL and DEL after the slash, `https:`,
`javascript:` and `data:` URLs, empty and relative paths; 14 legitimate: queries,
fragments, `/%2F/…`, `/.//…`, spaces, non-ASCII, a backslash later in the path) 11
hostile ones left the site or kept a control character before, and after all 37 are
handled (hostile paths become `/`, legitimate ones are preserved byte for byte); the
callback answered `307 Location: http://evil.example/steal?github_error=…` for a hostile
signed state before and `http://localhost:3040/?github_error=…` after, and the connect
route signed `//evil.example/steal` before and `/new` after. Not rolled.

## 2026-10-13 — the dashboard's TV stream proxy can no longer be pointed at a private address

`GET /api/tv/hls?url=` (`ui/app/api/tv/hls/route.ts`) fetched any `https:` URL for any
caller (`proxy.ts` leaves `/api/*` public): a visitor could make the dashboard node dial
loopback, RFC1918, CGNAT or link-local (`169.254.169.254`) hosts and read the answer back,
follow a public redirector to a private address (undici followed a 5-hop chain and an
`http:` hop) and pull unbounded bodies through the node's egress (found by the ui
comment sweep). Every outbound fetch now goes through one server-only primitive,
`safeGet` (`lib/safe-fetch.ts`): the address check is the connect-time `lookup` of
`node:https`, so the check and the connection share one resolution (no pre-check followed
by `fetch`, no DNS-rebinding window) and ONE non-public address among the answers refuses
the target; IP-literal hosts (the URL parser has already normalised the decimal, hex,
octal, short and bracketed forms) are validated before dialing; `localhost`,
`*.localhost`, `*.internal`, `*.local` and single-label names are refused; only https
without userinfo; redirects are followed by hand, at most 3, every hop re-validated. The
policy is `isPublicAddress` (`lib/public-address.ts`): all IPv4 special-use ranges, IPv6
only inside 2000::/3 minus 2001::/23, 2001:db8::/32, 2002::/16 and 3fff::/20, and the
IPv4-mapped and NAT64 forms unwrapped to the IPv4 check. The route keeps its contract
(playlists rewritten to point back at it, now against the FINAL url after redirects) and
adds bounds: a playlist is at most 2 MiB and must start with `#EXTM3U`, every other body
at most 64 MiB (declared length refused up front, the stream and the upstream socket torn
down past the cap or when the client cancels), content types limited to HLS playlists,
`video/*`, `audio/*`, `application/octet-stream`, `binary/octet-stream`, `application/mp4`
and `text/vtt` (else 415; a `.ts` served as `text/vnd.trolltech.linguist` or `text/plain`,
the IANA mapping of that extension and 10 of 600 surveyed catalog channels, is relabelled
`video/mp2t`), only a single `range` forwarded (never cookies or authorization),
`nosniff`, a 25 s total deadline and a 10 s socket idle timeout. The two GitHub REST
helpers (`gh` in `lib/github-rest.ts`, `appFetch` in `lib/github-installation.ts`) lost
their `path.startsWith("http")` branch, which no caller used and which would have sent
the user or App token to a caller-chosen host. Witnessed through `next dev` with
recording listeners on 127.0.0.1 and ::1: of 43 hostile URLs (loopback v4/v6, `localhost`
forms, decimal/hex/octal/mapped/NAT64/6to4 literals, `127.0.0.1.nip.io` and
`localtest.me`, RFC1918, CGNAT, metadata, multicast, reserved, userinfo, non-https, and
public `httpbin.org` redirectors to loopback with 301/302/303/307/308) 20 reached a
listener before the fix (40 connections, the old code retrying once); after the fix all
47 table rows are refused with 400/403 (the 5-hop chain is a 502) and the listeners
recorded zero connections, and the same holds for redirects served from a synthetic
public origin through a cloudflared quick tunnel (53 bound checks: header forwarding,
range, rewrite against the final url, 2 MiB and 64 MiB caps, content-type refusals and
relabels, client-cancel teardown of the upstream socket, idle stall). The feature still
works: of 60 random iptv-org catalog channels 47 playlists were proxied and rewritten and
36 first segments streamed before, 46 and 35 after (two `stream8.cinerama.uz` channels
whose origin started answering this IP with a 302 to a "blocked" slate between the runs,
curl sees the same; one flapping channel came back), and a 300-channel run played 168
first segments, including the three `.ts`-as-`text/vnd.trolltech.linguist` channels the
bare allowlist had refused. One catalog host,
`folxsloapp.b-cdn.net`, publishes an A record of 127.0.0.1 and used to be dialed on the
node's own loopback. Not rolled.

## 2026-10-13 — three secret comparisons now run in constant time

The mesh hot-join proof (`join_handler` in `main.rs`: `HMAC(HIVE_JWT_SECRET, the
caller's own endpoint id)`), the sandbox internal-hop token (`require_internal_hop`,
`x-hive-internal` against `HIVE_INTERNAL_TOKEN`) and the SMS verification code
(`verify_sms`) were compared with `==`/`!=`, which exits on the first differing byte,
while every other secret or MAC compare in `hive-cloud` already used `admin::ct_eq`.
The join proof is the one that mattered: it is the HMAC of an endpoint id the caller
chooses, so a caller able to time the comparison could recover a valid proof for its
own id byte by byte and admit itself into the trust set (hard over QUIC, but the
textbook reason for a constant-time MAC compare). All three now use `ct_eq`. A scan of
`hive-cloud`, `hive-core`, `hive-p2p`, `hive-browser` and `hive-zkauth` found no other
comparison of an attacker-supplied secret: the rest compare ids, hostnames, content
digests and public Fiat-Shamir values, and the API-key lookup compares the SHA-256 of
the presented key, not the key. Witnessed on a throwaway three-node loopback mesh over
real iroh with peer trust enforced: a node holding the fleet secret is admitted by
`join_handler` (the joiner receives the roster and the admitting node's trust set grows
by one), a node holding a different secret is rejected with `invalid proof` and the
trust set does not change. Not rolled.

## 2026-10-13 — /v1/anycast no longer hands every signed-in user the mesh addressing /v1/nodes withholds

`anycast_table` (`admin.rs`) returned `c.registry.routing_table()`, the FULL `NodeInfo`
of every node, to any verified session (`anycast_public` only called
`require_auth_read`), while `/v1/nodes` serves non-operators a sanitized projection
without the dialable addressing: a hobby-plan tenant could read every node's `peer_id`,
`iroh_addr`, `guardian_iroh_addr`, `relay_url`, `public_ip`, `public_url` and
`private_addr` from `GET /v1/anycast` (found by the comment-sweep agent while reading
the handler). The non-operator projection is now ONE helper (`non_operator_node_view`),
used by `/v1/nodes` unchanged and by `/v1/anycast` plus `latency_ms`, the one extra
field the Network page reads; operators and dev mode keep the full table. `GET
/v1/ratelimit`, `GET /v1/concurrency` and `GET /v1/wqueue/stats` took no caller at all
and now require a session like the other topology reads (the first two are read by the
dashboard's Network page, deployment canvas and service graph; the third is
platform-wide world-queue counters with no tenant ids). Witnessed on throwaway nodes:
with auth enforced a tenant token's `/v1/anycast` rows carry none of those seven keys
and do carry `latency_ms`, an operator token still gets all 45 `NodeInfo` keys,
`/v1/nodes` keeps its shapes for both, no credentials is 401 on all four routes (admin
listener and public ingress), a tenant token gets 200 on the three counters and a
tenant `PUT /v1/ratelimit` is still 403; a dev node (no `HIVE_JWT_SECRET`) still
serves the full table and the counters without credentials. The other `get` routes
with no caller extractor (mesh liveness probe, static catalogs, the public status
board, share and checkout capability links) are public by design and unchanged. Not
rolled.

## 2026-10-13 — the runtime cache no longer answers requests with no credentials

`rc_authorize` (`admin.rs`) let any request without a JWT, API key, `x-hive-team` or
`Authorization` header through, on the premise that such requests are the in-cell
loopback data plane. The public `api.<platform_domain>` ingress serves the same admin
router and the auth middleware never rejects a read, so `GET
/v1/runtime-cache/entry?scope=<project>:<env>&key=<k>` with no credentials returned
any tenant's cached value, and `GET /v1/runtime-cache` served fleet-wide counters.
Witnessed on a throwaway node with auth enforced: tenant B's token got 403 on
tenant A's scope, a request with no credentials got 200 and A's value (found by the
comment-sweep agent while reading the handler). Under enforcement a request with no
credentials is now 401 (`require_auth_read`, the same contract as every tenant
read), another tenant's token stays 403 and the stats are operator-only; without
`HIVE_JWT_SECRET` (dev) the in-cell data plane stays open exactly as before
(verified: PUT 204, GET 200, DELETE 204). Anonymous PUT/DELETE were already 401 at the
middleware. Not rolled.

## 2026-10-13 — the relay certificate renewal script named a deleted host and skipped phoenix

`scripts/renew-relay-cert.sh` copies the `*.relay.shadw.app` wildcard to
`RELAY_NODES`, which listed fc-bangkok (deleted) and omitted fc-phoenix: run as
written it aborted on the dead host and left phoenix's relay on the old
certificate. Live check: fc-virginia, fc-sanjose and fc-phoenix all serve the same
certificate, which EXPIRES 2026-10-31 23:59:59 GMT (issued 2026-08-27; the script
is not scheduled anywhere), and `iroh-relay.service` is active on phoenix with
`/etc/iroh-relay/{cert,key}.pem`. `RELAY_NODES` is now phoenix, virginia, sanjose.
The renewal itself is not run: it is the operator's call and must happen before the
31st or every `https://<node>.relay.shadw.app:3343` relay stops terminating TLS.

## 2026-10-13 — the host build lane reads tenant output through the capped reader

`IsolatedBuild::run_host` drained a build command's stdout and stderr with
`BufReader::lines().next_line()`, which has no length limit: a step that prints a
long run of bytes without a newline (progress bar, one-line source map, a hostile
command) grew one `String` until the node died, and the store-boundary cap
(`MAX_BUILD_LOG_LINE_BYTES`) only bounded what the `Build` record kept afterwards.
Both readers now use `hive_core::logcap::read_capped_line` like the mock backend,
the cell agent and the isolated executor: the pipe is still drained, at most 16 KiB
of a line is retained, truncations are counted in `LOG_CAP_STATS`
(`GET /v1/debug/memory`). Witness (hermetic node, build script printing 256 MiB with
no newline): node RSS peak 644 MB (+548 MB) before, 140 MB (+28 MB) after. Not rolled.

## 2026-10-13 — a failed first build no longer hides its project or locks its name

A project whose first build failed had a `ProjectSettings` row (created at
admission by `git::start_build`) and no deployment, and every dashboard list
derived projects from `/deployments` alone — so the project was invisible, the
pending row vanished on `error`, and importing the same name again answered
409. Witnessed on the leader: 62 of 98 `personal` rows had no deployment; the
reporting user's tenant held exactly their two failed imports (`rest`,
`security`) plus one older row, none deletable or retryable from the UI.

- **Rows carry the newest build's outcome.** `ProjectSettings.last_build`
  (terminal states only, error capped at 400 bytes) is written from the one
  chokepoint every build exit crosses, `BuildCompletionGuard::drop`, which also
  finalizes a build record that never reported (Cancelled if the slot was
  cancelled, else Error); `last_build_id` records the admission and the live
  `BuildStore` decides "building"; `first_deployed_ms` is grow-only proof that
  a project ever deployed. Boot replays every terminal build record into its
  row, so builds that failed before the field existed are listed after a roll.
- **`GET /v1/gitops/projects?view=summary`** (`project_inventory.rs`): every
  row the tenant owns (`record_tenant` on the stored tag, never `norm`), one
  pass over gateway + peer deployments for counts, no relational tier, served
  through the leader with a 3 s budget then locally, `require_auth_read`, and
  a `gossip::dispatch` arm. The default view is byte-identical.
- **Re-importing the same repository under an own never-deployed name
  retries the project** (`start_named_deploy` → `project_inventory::name_reuse`):
  root dir and branch follow the new request, `inference` resets, env/domains/
  owned resources stay; an in-flight build answers 409; a different repository
  keeps the 409 with text that names the retry/delete paths. Ownership is a
  compare-and-set (`claim_team_exact`) so two tenants cannot both claim one
  new name. `/v1/projects/:p/redeploy` falls back to the last build's source
  (never `upload://`); webhook and git-poll still ignore never-deployed rows.
  `HIVE_MAX_EMPTY_PROJECTS` (25) bounds never-deployed rows per tenant (429).
- **Dashboard**: `lib/projects.ts` merges the inventory into the deployment
  list (deployment first, keyed by name, ordered by activity); cards show
  building/failed/no-deployment states with the error's first line; the
  project page renders an empty-project card (error, build-log link, Retry
  via redeploy, Delete, settings links) instead of a spinner and a dead Visit
  button; `/deploy/[id]` links a failed build to its project. CLI:
  `shadw deploy`/`build logs --follow` exit 1 on error/cancelled with the
  retry/delete hint, `projects list` gains a LAST BUILD column.
- Not rolled.

## 2026-10-13 — san-jose builds died on a dead resolver behind the provider npm mirror

Every uncached `npm ci` / `npm install` on fc-sanjose failed: `npm ci` with
npm 10's `Exit handler never called!` after ~70 s, `npm install` with
`ENOTFOUND mirrors.tencentyun.com`. Root cause: the node's `/etc/resolv.conf`
had been overwritten four days earlier (`nameserver 1.1.1.1`, a litebox
guest-DNS probe-session leftover) while `/root/.npmrc` points npm at
`https://mirrors.tencentyun.com/npm`, a name only the provider's resolvers
answer. The host build lane forwards HOME (the isolated lane does not), so the
mirror configured every host-lane install; installs whose dependency set was
warm in `~/.npm/_cacache` kept passing, which hid it. Reproduced red in a
private mount namespace (host file untouched) and green after
`nmcli general reload dns-rc`; real platform builds of vercel/vercel
`examples/nextjs` and `examples/node` reached Ready on san-jose.

- **Host-lane registry guard** (`build_registry.rs`, wired in
  `IsolatedBuild::run_host`): the host registry is probed once per build
  (every address dialed concurrently, 3 s); only an EXACTLY allowlisted
  public-mirror host that is dark while registry.npmjs.org answers changes the
  build, and the change is a user-config file, not a registry variable: a 0600
  copy of the host `~/.npmrc` with its registry replaced by the public one,
  handed to the commands as `npm_config_userconfig`. That layer sits below a
  repository's own `.npmrc`, so a tenant registry wins by npm's own layering
  with no detection (the first cut detected tenant config by walking up from
  the command cwd and failed OPEN for `cd server && npm i`, symlinked or
  oversize configs; and pnpm 11+, which the fleet pins at 12.6.0, ignores
  `npm_config_registry` altogether). Verified on fc-sanjose under a dark
  mirror: npm 10 `install`/`ci` and pnpm 11 succeed against npmjs, a project
  `.npmrc` naming a dead private registry still fails against THAT registry.
  bun ignores user config (no fallback, by design); a lockfile resolved
  against the dead mirror still fails `npm ci`. Build-log URL is scheme://host
  only. `HIVE_BUILD_REGISTRY_GUARD=0` disables, `HIVE_BUILD_REGISTRY_MIRRORS`
  extends the exact-host list.
- **Node-server detection** (`fluid_build::framework::generic_node_server_entry`):
  Vercel's generic `node` preset — `server.{cjs,js,mjs,mts,ts,cts}` at the
  root or `src/` — when the file actually serves (`listen(`/`createServer(`/
  `serve(`) and the repo shows no static intent (no root `index.html`, no
  `vercel.json` `outputDirectory`), so `examples/node` (a bare
  `http.createServer`, no framework dependency, no start script) is served
  instead of shipped as a Static site that answered `NOT_FOUND`, while a
  static site with a preview `server.js` or an exports-only library stays
  Static. `app_discovery::classify` counts the same entry as evidence.
- **Worker lane no longer hijacks Node apps**: `discover_worker_entry`
  requires a wrangler config; without one it probed the same `index.*`/
  `main.*` stems and sent plain Node apps through the Worker adapter (exit 1
  "no Worker fetch handler"). TypeScript entries run under
  `--experimental-transform-types` (enums, namespaces, parameter properties).
- **Express launcher** (`assets/express-launcher.mjs`): never binds a second
  time when the entry already listens (was `ERR_SERVER_ALREADY_LISTEN`), and a
  self-listening entry that binds late is polled every 250 ms up to
  `HIVE_LAUNCHER_LISTEN_DEADLINE_MS` (60 s) instead of a single probe at 1.5 s.
- **Audits**: new `scripts/audit-build-registry.sh` (DNS/TCP/HEAD of each
  node's configured registry plus its resolvers — a HEAD, because the Tencent
  mirror ignores `Range` and a GET pulls the 21 MB packument; 2xx/3xx/401/403
  all count as "answers"; IP-literal and userinfo registries handled; a node
  without npm is its own finding). `audit-public-listeners.sh` reads every
  lockdown LAYER through nft (the legacy `ip filter HIVE_LOCKDOWN` chain and
  the `inet hive_lockdown` table), compares each layer's peers with the live
  inventory as a SET (names the missing and extra addresses, never a count),
  flags a node carrying both layers, skips `hive_retired` hosts like the
  roster generator, and fails closed: a mistyped `ONLY`, an empty selection
  or a host it cannot audit (non-root ssh) is never a green verdict
  (`ALLOW_SKIPPED=1` accepts the latter). Live finding: every node enforces a
  6-peer roster against 7 expected — fc-orlando (209.23.0.183) is missing
  from all five.
- **`hive-lockdown.sh` retires the legacy chain.** The nft-first branch left
  the iptables-era `ip filter HIVE_LOCKDOWN` chain in place beside the new
  table; both hook INPUT, so the effective policy was their intersection and
  a peer added to the roster stayed dropped by the stale chain while the audit
  read the new table as healthy (reproduced in scratch network namespaces).
  After the new table is live the script deletes every `jump HIVE_LOCKDOWN`
  from the vendor table, flushes and deletes the chain, and fails loudly if
  it survives; a host without the chain is untouched (idempotent, verified in
  the same namespaces for the sj/va/va3 shape, the phoenix shape, a re-run,
  and a chain entered from two jump rules). Rolling it is the operator's call.
- Needs a fleet roll to take effect (backend only; `ui/` is unaffected).

## 2026-09-24 — CS-7 gossip rounds never wait on dead peers

fc-virginia's gossip round was `join_all` over every target, so it lasted as
long as its slowest: the powered-off fc-bangkok seed cost 4 serial 5 s dial
timeouts per round (the announce twice, then the mesh join twice) and the
round ran 25-33 s. Every relayed `last_seen_ms` is only as fresh as the
round is short, so fc-sanjose — which fc-virginia could not dial, but three
other nodes heard throughout — aged past `GOSSIP_ALIVE_MS` (25 s) and was
demoted 57 times in a day, each time handing fc-virginia a different
control-plane owner.

- **Round deadline** (`HIVE_GOSSIP_ROUND_DEADLINE_MS`, default 8000): each
  target syncs in its own task; the round merges what arrived by the
  deadline. A straggler keeps running under a per-target in-flight flag (not
  re-dispatched until it finishes) and its result lands in whichever round
  receives it. The task reports through a drop guard, so a panicking sync
  cannot leave its target flagged forever.
- **One bounded fan-out for both round loops.** The deadline, the in-flight
  flags and the drop guard are one primitive, `bounded_round::BoundedRound`,
  and the health prober runs on it too: a failing peer's probe (two samples
  at the dial-fallback ceiling, ~28 s) no longer holds every probe round, the
  restore hysteresis and every other peer's verdict. A probe round waits
  `health::probe_deadline()` (2 × `HIVE_HEALTH_TIMEOUT` + 1 s). A round never
  waits for an earlier round's straggler, so one peer that is slow every time
  cannot pin every round to the deadline.
- **A slow peer is late, never absent.** The gossip loop keeps each target's
  last answered sync and folds it while the next one is in flight and the
  node is still in the registry, so container holders (the lease HRW set),
  routes, fleet deployments and the zkauth peer roster (now swapped whole by
  `zkauth::set_peer_exports` instead of cleared at round start) keep a live
  peer whose sync outlasts the deadline. Contributions are deduplicated per
  node, newest round first (a seed is both `seed:<eid>` and `<eid>`). A
  target that failed contributes nothing, as before.
- **Dead-target backoff, from a fresh view only.** A target with no
  successful sync AND no gossip of it in the registry for 10 min is
  dispatched on a 1 → 3 min backoff instead of every round; any answer clears
  it. The backoff only runs while this node itself reached some target within
  the last 60 s: an isolated node sees every target silent, seeds included,
  so it keeps dialing all of them every round, and the first answer after a
  blind stretch clears every backoff. The 3 min cap matches the mesh's bound
  on anything being undialable. A peer another node still hears is never
  backed off (a `--peer` URL target resolves that through the node id it last
  answered as), and `PeerPool::acquire` is untouched.
- **One dial per dead target per round.** `gossip::fetch_outcome` classifies
  a failed fetch; the mesh join is skipped when the announce's own dial
  failed (budget elapsed, connect-phase `DeadPeerTimeout`, `PeerRefused`),
  and a target reached by nothing (announce, roster, join) skips its
  remaining reads instead of paying a 4 s HTTP timeout per read.
- **Round telemetry.** Every 10 min: `gossip rounds (last 10 min)` with the
  start-to-start round period p50/p99, the collect phase and the per-target
  resync interval p50/p99, deadline hits, stragglers and backed-off targets;
  a WARN when `GOSSIP_ALIVE_MS < 2 × p99 round period +
  health::probe_period_bound()`. The collect phase is capped by the deadline,
  so it is reported but never used for the margin. The same figures are
  `gossip_rounds` in `GET /v1/mesh/health-guard`.
- **Two consecutive stale rounds before a withdrawal.** The gossip loop
  counts, per peer, the consecutive round ends at which its gossip was stale
  (`health::note_gossip_round`); `health::demote` writes `healthy=false` only
  at `DEMOTE_STALE_ROUNDS` (2) — before that the new `PeerDemotion::Deferred`
  outcome marks the peer cold and nothing more (`deferred_stale` counter).
  The registry evaluates the gate under the same write lock as the verdict
  (`NodeRegistry::demote_if_gossip_stale_gated`). While the gossip loop has
  stopped ending rounds (before its first round, a whole-runtime freeze,
  measured on a monotonic clock) demotion HOLDS, because every peer looks
  stale from a registry nobody refreshes; the first round after a stall
  restarts the count.

Verify: on fc-virginia, `deadline_hits` and the round-period p99 stay low
enough that the margin WARN is silent, `backed_off_targets` names only
fc-bangkok-like dead seeds, 0 `peer marked UNHEALTHY ... fc-sanjose` over
24 h, and sj's `last_seen_ms` age in fc-virginia's `/v1/nodes` under 20 s.
Once the CS-3 control-plane lease has shipped, that margin is what allows
lowering `HIVE_CP_LEASE_TTL_SECS` to 60.

## 2026-09-24 — CS-2 transport liveness and budgets: a wedged endpoint can no longer refuse the fleet for hours

The wedge behind three incidents in 12 days (fc-virginia 09-12 and 09-18,
fc-sanjose 09-24): iroh's layer above noq stops completing connection setup,
warm trunks keep serving, and every peer-count watchdog stays quiet. hive-p2p
held each inbound budget permit across that unbounded await, so the budget
filled with permits of dead connections and the endpoint refused the whole
fleet (335,055 refusals on sj in a day) until an operator restarted it.

- **Accept deadline** (`HIVE_P2P_ACCEPT_DEADLINE_MS`, default the 30 s QUIC
  idle timeout + 10 s): an accept whose handshake + iroh registration
  outlives it is CLOSED with the overload code — releasing its budget slot,
  and making the dialer back off instead of holding a trunk that never
  serves a stream — and counted as `accept_stuck` (WARN at most once per
  10 s). Above the idle timeout, a handshake the remote abandoned ends as a
  handshake error and is never counted. In-flight accepts are tracked so
  `accept_inflight_oldest_ms` is reportable.
- **Budget by negotiated ALPN.** A ClientHello that does not fit the first
  Initial — every hybrid X25519MLKEM768 dial — is no longer charged to the
  128-slot browser budget: it holds a `pending` slot
  (`HIVE_P2P_PENDING_MAX_CONNS`, 256) across the bounded handshake, then
  `conn.alpn()` moves it to its real budget or closes it with the overload
  code. Fleet connections are capped per remote endpoint
  (`HIVE_P2P_MAX_CONNS_PER_ENDPOINT`, 16) at admission; established trunks
  are never evicted. Budget-exhausted and per-endpoint-cap WARNs are
  rate-limited and carry counts and the remote.
- **Streamed gossip responses.** The response length is read under the 15 s
  first-byte budget and the body in 64 KiB reads each under the 45 s idle
  budget (gossip and join), instead of the whole frame inside 15 s — which at
  ~170 KB/s per trunk made any snapshot over ~2.5 MB impossible to fetch.
  Replies of 256 KiB or more go out below the default stream priority, so
  probes on the same trunk are not starved behind a snapshot.
- **The follower pull is its own DB-free loops** (`store_follower`):
  `store-follower-sync` pulls small stores concurrently every
  `HIVE_STORE_SYNC_SECS` (60) and `store-large-lane` pulls large ones (over
  1 MiB — billing, incidents, audit — or SUSPECT: a batch fetch that ran out
  its whole budget while others fetched) one at a time in its own task,
  holding off while the link is down. Neither waits on GuardianDB or the
  relational mirror any more (they ran after `relational::sync_deployments`).
  Large stores are pulled at most once per 5 min after a success and retried
  after 60 s after a failure; any other batch failure retries next tick.
  One shared transfer primitive (`store_sync::fetch_snapshot` /
  `fetch_budget`) sizes every snapshot fetch — ≤1 MiB: 10 s / 16 MiB;
  larger: size + 25 % at 64 KB/s + 30 s, floored at
  `HIVE_STORE_SYNC_LARGE_TIMEOUT_SECS` (240), 64 MiB bound — for the
  follower pull and `reconcile_on_promotion` alike; promotion now pulls
  large stores one store at a time under that budget and never adopts a
  wholesale store whose local copy changed during its fetch. A pull that
  outlived the owner it was resolved for is not adopted.
- **Refused-dial backoff and wrong-identity hints.** A refused dial opens a
  per-endpoint 1 s → 30 s window in which `acquire` fails at once with
  `PeerRefused` (not a dead-peer signal) instead of re-dialing into the
  refusal; a round-tripped request clears it. A pre-handshake refusal from a
  hint's direct addresses opens it only once the relay (else fresh
  discovery) confirms it — it names no identity. A dial that fails
  `UnknownIssuer` suspends every direct address of that hint for the peer
  (10 min) and retries through its relay at once — only while the hint has
  a relay; the dial set is never emptied.
- **Establishment stats and the `establishment_wedge` watchdog.**
  `hive_p2p::establish_stats` (accept stuck/in-flight, last fresh inbound and
  outbound establishment, windowed attempt/initial/refusal counts, per-class
  budget use, top-10 budget holders) is served on operator-only
  `GET /v1/mesh/establish` (`/v1/mesh` keeps its public shape). meshwatch's
  new trigger fires the controlled exit-17 on evidence no remote can forge:
  no fresh connection either way for `HIVE_MESH_ESTABLISH_WEDGE_SECS` (300)
  while dials from here to two or more directly-heard peers timed out, or 3+
  stuck accepts with none succeeding corroborated by dead outbound setup;
  refusals and negative-discovery short-circuits never count, and stuck
  accepts alone only WARN. Also: 10 min uptime, an audible fleet, the node's
  stagger. `HIVE_MESH_ESTABLISH_WEDGE_RESTART=0` makes it WARN-only.
- **One restart chokepoint.** `ControlledRestart::request` refuses every
  automatic restart but memory pressure until the cell-orphan reap confirmed
  this boot (all mesh triggers, not only the new one — WARN-only on macOS and
  with `HIVE_CELL_ORPHAN_REAP=0`), and applies each reason's rate cap
  (`establishment_wedge`: 1 per 6 h); a refused trigger keeps watching.
- **Controlled restarts record their reason; the cap fails closed.**
  `ControlledRestart::request` stamps the reason into the run marker when
  requested, so the next boot's `restart_history.json` record carries
  `reason` (`memory_pressure`, `mesh_isolation`, `mesh_degradation`,
  `establishment_wedge`). When the history could not be read or parsed at
  boot (it is set aside), or this boot's record or the latest run marker
  could not be written, the count is unverifiable and the cap is treated as
  exhausted.

## 2026-09-24 — CS-1 safety floor: one gate for every leader-only job, no self-election, no leaked cells

The fc-virginia instability (three wedges in 12 days; the 2026-09-24 one on
fc-sanjose) did its damage through local mechanisms that any flap, restart or
roll could re-trigger. This change set makes the next flap, restart or roll
harmless; transport liveness, the signed control-plane lease and DB-free
replication follow as their own change sets.

- **One gate for every leader-only background job** (`leadership.rs`). Node-
  death relocation, git poll, billing meter (its relational projection and
  the token-mint enterprise lock), Vercel DNS, ACME DNS-01, custom-domain
  HTTP-01 (its own designation, the `acme_http01` store's writer), domain
  verify/pin, push dispatch (and the fleet VAPID keypair), inference env
  injection, guardian reap, relational backfills, browser admission/presence
  expiry and listener-audit incidents each carried their own leadership test
  of different strength. They now all ask `leadership::may_act(cloud, Job)`:
  owner, a fresh view (a gossip round done since boot), not isolated, and —
  for a BACKUP chain owner only — direct contact (outbound dial or inbound
  self-report) with a majority of `HIVE_CP_VOTERS`, all held continuously for
  the job's minimum tenure (relocate 600 s … others 30 s). The chain head acts
  on presence, so a head reachable only through relays keeps writing instead
  of freezing every job fleet-wide. `HIVE_CP_VOTERS` unset under a chain is an
  ERROR at boot (no derived voters); a chain-less node with a mesh roster
  HOLDS instead of electing itself. Each ownership run has a term; the DNS
  reconciler's take-over work keys on it, and the promotion reconcile runs
  once per continuous request-path ownership — neither re-fires on a quorum
  or isolation blip. Cron and raw-port allocation stay on their current gates
  until they have replicated stores.
- **Presence, not health; HOLD, not election.** With `HIVE_CP_OWNER_CHAIN` set
  the owner is the first chain entry present in the gossip-fresh set with an
  identity and a public address; the observer's `healthy` flag is not
  consulted and the identity-election fallback and the
  `HIVE_CP_LEADER`/`HIVE_DNS_LEADER_NODE`/`HIVE_BILLING_COORDINATOR_NODE` pins
  are never used while a chain is set. A chain dark in a node's view is a
  HOLD. `control_plane_leader()` now returns `Option` and never resolves to
  the calling node unless it is the real owner; every request-path proxy asks
  `CloudState::leader_forward_target()` once (local best-effort reads,
  retryable 503 writes when there is nobody else to ask).
- **Incidents dedup at the primitive.** `IncidentStore::open` returns (and
  touches) an unresolved incident with the same title and affected set,
  atomically; only the operator's `POST /v1/incidents` opens unconditionally
  (`open_new`). fc-virginia opened 3.4k–12.5k duplicate self-heal incidents a
  day, and the DNS hold incidents re-armed on every leadership edge.
- **Node-death self-heal is off by default** (`HIVE_NODE_DEATH_SELF_HEAL=1`
  opts in). Enabled, it keys on the host's absence from the fleet's gossip
  for 10 min, measured from this node's first observation made from a fresh
  view (clocks are discarded while this node is isolated, gossip-blind or
  short of a voter majority), a 6 h per-(project, host, commit) cooldown and
  3 relocations per hour per node.
- **Cell-orphan reap and a per-volume writer guard** (`hive_backend::cell_orphans`).
  Every cell is labelled `hive.owner` (stable per node + data dir) /
  `hive.boot` / `hive.cell` / `hive.node`. At boot every cell of THIS node from
  another boot is removed — never another co-hosted process's cell; label-less
  legacy cells only under systemd or `HIVE_CELL_ORPHAN_REAP_LEGACY=1`. Duplicate
  writers of a shared volume are SIGKILLed except the newest, which is
  stopped gracefully last; boot waits at most 20 s for the listing and the
  kills, the rest finishes in the background. Until a re-list confirms the
  reap, each launch mounting a `hive-vol-*` first stops and removes a live
  (running, paused or still stopping) stale writer of that volume in a
  detached task and fails with a node fault if one survives. Unparseable
  podman output skips the reap loudly. `orphan_reap_ran()` (the interlock for
  restart-based remedies) is true only after that confirmation, never on
  macOS. `HIVE_CELL_ORPHAN_REAP=0` opts out.
- **Loud follower pulls, wholesale only from the chain head.** A failed
  store-snapshot fetch WARNs per (leader, store), opens a deduped incident
  after 10 consecutive failures (one "every store" incident when the whole
  link is down) and resolves them on success, on a leader change, or when
  this node becomes the owner. Wholesale stores are adopted only from the
  chain head (or the chain-less election); a backup or fallback owner is
  pulled for merge stores only.
- **`HIVE_CP_VOTERS` ships through ansible** (`hive-node.service.j2`,
  `hive_cp_voters` in inventory `[platform:vars]`).

## 2026-09-22 — the mesh QUIC transport now offers hybrid post-quantum key exchange (X25519MLKEM768)

The mesh transport (`hive_p2p::bind_full`, plus GuardianDB's own separate iroh
endpoint) compiled with only the classical `ring` TLS backend up to this
point: `iroh = "1"` with no feature override pulled the vendored iroh crate's
own default `tls-ring`, and zero post-quantum key exchange was offered by any
node (`docs/pqc-migration-scope.md`, written 2026-07-20, correctly described
this as unimplemented). Fixed: `tls-aws-lc-rs` is now enabled alongside
`tls-ring`, and `bind_full` (and guardian-db's endpoint) explicitly call
`.crypto_provider(...)` with `X25519MLKEM768` first — mandatory, not optional,
since iroh's own presets prefer plain ring whenever both TLS backends are
compiled in (which Cargo feature unification now makes true workspace-wide;
relying on the feature flag alone would have shipped nothing).

Real negotiated-handshake telemetry (`hive_p2p::pq_kex_stats()`, exposed via
`GET /v1/relay`'s new `pq_kex` block) reads the actual completed handshake's
key-exchange group off each connection — never inferred from a config flag.
Live-verified both directions with `crates/hive-p2p/src/bin/p2p_demo.rs`
(committed tree unchanged — the print statements used were added, run, and
reverted): two hybrid-capable endpoints negotiated `X25519MLKEM768` over a
real QUIC tunnel carrying real HTTP traffic; a peer forced onto plain `ring`
(zero PQ support, simulating an unrolled binary) still connected and served
traffic normally via automatic TLS group fallback, correctly classified as
`classical_fallback` rather than `hybrid_pq`.

Transport IDENTITY is unchanged and stays classical Ed25519 — ML-KEM is a
key-exchange primitive only. Public dashboard/API HTTPS, DB gateway TLS, and
the relay binaries' own outer TLS were deliberately left on `ring` (external
client compatibility, not fleet-controlled peers); `hive-browser` (wasm32)
cannot use aws-lc-rs at all (no `wasm32-unknown-unknown` support) and is a
permanent boundary, not a gap. Full audit, including a re-verification of
`docs/COMPLIANCE_AUDIT.md`'s identity/authz findings and the actual scope of
discovery (DHT/pkarr/relays real; Bluetooth absent; mDNS real but scoped to
GuardianDB's own LAN replication mesh only) against current code:
`docs/security-architecture-audit-2026-09-22.md`.

## 2026-09-22 — the litebox orphan reaper missed every orphan created by an ordinary runner deploy

`LiteboxBackend::reap_orphaned_runners` (boot-time, SIGKILLs any process whose
`/proc/<pid>/exe` is the runner binary) compared that readlink by plain string
equality. Replacing `/usr/local/bin/litebox-runner` — what every deploy does;
Linux refuses an in-place overwrite of a running executable (`ETXTBSY`), so the
role and this platform's own ship scripts both `mv` a new file onto the path —
unlinks the OLD inode while an existing orphan still has it mapped, and the
kernel then appends `" (deleted)"` to that orphan's `/proc/<pid>/exe` from then
on. The comparison silently stopped matching for exactly the orphans a binary
swap creates. Two fc-phoenix runners from an earlier incarnation survived two
restarts this way, burning ~48% of a core apiece for nearly an hour before
being killed by hand.

`strip_deleted_exe_suffix` undoes the annotation before comparing. Confirmed
the kernel behavior directly (`cp`+`mv -f` over a running `sleep` on va: its
`/proc/<pid>/exe` read back with the suffix). Reproduced the bug for real on
fc-phoenix — hard-killed hive-node, swapped the runner binary the same way a
deploy does, confirmed the two now-orphaned runners read `(deleted)` — then
restarted onto the fixed binary: `reaped=2`, both gone, zero orphans left.
Rolled to fc-sanjose next, which had accumulated `reaped=7` from tonight's
earlier restarts; both survey apps and tokenhun cold-started normally
afterward. Every litebox runner on both nodes is now a child of the current
hive-node process.

## 2026-09-22 — litebox `fork()` root-caused and fixed: a second external command no longer wedges a sandbox shell

The wedge behind the `ifconfig` report was not the not-found path: on a native
Linux runner a plain `fork()` took `do_clone`'s eager copy of the guest's writable
memory at RELOCATED host addresses, run on a second host thread. Only registers,
the TCB pointer, a 4 KB stack window and a few ELF data words are patched;
everything else in the copy (RELRO/`.got`/`.data.rel.ro`/stdio vtables — Rocky's
bash is full-RELRO — and the whole heap) still points into the parent. The child ran a
mix of its own and the parent's libc and rewrote the parent's `_rtld_global` stack
lists, malloc and stdio state. gdb evidence: `_IO_vtable_check` failing in the child
with `rip` in the PARENT's libc (`glibc detected an invalid stdio handle`), and the
second child spinning in `__libc_fork`'s inlined `reclaim_stacks` walk over the
list the first child had corrupted.

`ansible/roles/litebox/files/fork-inplace.patch` (applied after `networking.patch`;
`litebox_shim_linux` only, 180 lines) runs a plain `fork()` of a single-threaded
caller in place, vfork-style (caller suspended until the child execs or exits, no
relocation, writable memory restored from a snapshot afterwards); multi-threaded
callers (Node) keep the old path. It also fixes `sys_brk` (a `brk` that cannot be
satisfied answers the unchanged break, not `-ENOMEM`) and serves syscalls at the
rewriter's `icebp; hlt` trap sites through the shim (they killed `head` in 25-30% of
`ls | head` runs). Matrix on a scratch runner on va: pin runner wedges after the first
command; the fixed release runner (`cdb25f95`) passes `id`,`id`,`uname -a`, a
not-found command through bash's own path, `ls / | head -2`, `v=$(echo hi)`, `id -u`
(100-iteration loops of the pipeline cases, 20 of the rest, all clean). Live on
fc-sanjose through the real dashboard shell and one-shot exec paths: `id`,`id`,
`uname -a`, pipelines, command substitution, `node -v` + `node -e` in one session,
`sh -c 'ifconfig; id; nothere2; echo end'` (was fatal), all correct. Installed as
`/usr/local/bin/litebox-runner` on fc-phoenix and fc-sanjose (old ones kept beside
it); tokenhun (a Next guest) cold-started on it and answers 200.

Known limits (documented in PATCHES.md): the parent is suspended until the child
execs or exits, so a non-exec child that needs the parent deadlocks (`$(...)` of more
than 64 KiB written by builtins); `(sleep 1; echo x) & echo y` prints `x` first; O(RSS)
per fork. The `litebox-shellrc.sh` no-fork guard from the same day stays until every
litebox node runs this runner. Also found: rolling a hive-node restart leaves the
old litebox runners running as orphans (ppid 1, ~50% CPU each on phx) alongside the
duplicates the new process starts — two killed by hand on phx, PRD row open.

## 2026-09-22 — a command the sandbox terminal cannot find wedged the whole session (`glibc detected an invalid stdio handle`)

Typing `ifconfig` (or any command not staged into the guest tar: `ip`, `ping`,
`curl`, `vi`, `less`, `ps`, …) in a dashboard sandbox terminal on a litebox node
printed `Fatal error: glibc detected an invalid stdio handle` and
`sh: [pid: 1 (255)] tcsetattr: Inappropriate ioctl for device`, after which the
session answered nothing. Reproduced on fc-sanjose through the real shell
websocket. Interactive bash forks BEFORE it looks a command up (so it can
redirect the error), and under litebox's fork emulation that child dies on its
first stdio use; the shell never recovers.

The staged `ENV` rc (`litebox-shellrc.sh`, was one line `exec 2>&1`) now also
installs an `extdebug` DEBUG trap that checks the command word with
`command -v` in the PARENT and prints `sh: <cmd>: command not found` there — no
fork, no fatal. Leading `VAR=value` words are skipped over; builtins, keywords,
functions, `[[`, `((`, `cd`, `for`/`if` bodies are untouched. Witnessed on a
scratch runner on va (11 command forms, including `FOO=1 nothere`,
`echo a; nothere; echo b`) and live on fc-sanjose (`ifconfig` and `nothere` →
"command not found", `id` and `echo` still work, `exit` → exit_code 0). Known
limits: a skipped command leaves `$?` at 0, a quoted/expanded first word is not
checked, and the one-shot `sh -c` exec path is unchanged. NOT fixed here: any
SECOND external command in a session still wedges it (the native-Linux fork
emulation bug, `litebox-fork-child-corruption`); under investigation.

## 2026-09-22 — laptop `fc-lax3` dead for 59 h (launchd could not spawn it) and four dev/Mac nodes running without a trust list

The mesh's `fc-lax3` is the laptop's `dev.shadw.fc-lax` job. It had been dead
since 2026-09-19 01:01 with `last exit code = 78: EX_CONFIG`, 21,500 spawn
attempts, and this repo's watchdog kickstarting it every minute:
`launchd: Service could not initialize: Unable to get updated LWCR` (the
launch-constraint record went stale after the debug binary was rebuilt). The
same binary ran fine when launched by hand; `bootout` + `bootstrap` of the same
plist cured it. `scripts/shadw-watchdog.sh` now detects `last exit code = 78`
in its down branch and re-registers the plist instead of kickstarting forever.

Separately, `shadw2`, `shadw3`, `fc-lax` and `fc-lax2` carried no
`HIVE_TRUSTED_NODE_IDS` / `HIVE_PEER_TRUST` (`shadw1` did and was healthy), so
they rejected every mutating gossip (`no verified+trusted signer`, 257 in 3000
log lines on shadw2), saw `control-plane owner chain has NO eligible entry`
(1915 of 3000 lines) and reported `expected_peers` 0-7 against the fleet's 26 —
"connectivity issues" that were configuration. `fc-lax`/`fc-lax2` also still had
bare-id bootstrap seeds and the dead `http://<ip>:3340` relay form. All four now
have the 27-id fleet trust list (plists backed up as `*.bak-trust-*` /
`*.bak-mesh-*`), `https://*.relay.shadw.app:3343` relays and addressed bootstrap
peers, and report `expected_peers` 26-28 with 6-9 visible healthy peers.

## 2026-09-22 — a failed iroh rebind was never retried: fc-sanjose sat mesh-dark for 28 hours

`fc-sanjose` (control-plane leader) reported `isolated: true`, zero direct peers,
and ~25 `netwatch::udp: socket closed` warnings per second. The first failure was
`iroh::socket: failed to rebind transports: Os { code: 98, kind: AddrInUse }` at
2026-09-20 22:44:24 +08, seconds after a minecraft cell's podman
died/remove/create. `netwatch` drops the old UDP socket BEFORE binding the new
one, and iroh only logged the failure: the transport stayed closed until the
next major link change happened to rebind it (the v4 socket came back 11 h
later, v6 17 h later). With `HIVE_IROH_PORT` pinned, the bind fails whenever a
concurrently forking child (podman, conmon, git ls-remote polls, litebox
runners) still holds its copy of the old fd between `fork()` and `exec()` —
long on a host at load 20-39. A restart reproduced it 20 s after boot
(19:29:06Z). `meshwatch` never restarted it: the live fleet is 5-6 peers, below
the `expected/4 = 6` floor its `ever_converged` guard needs, and the continuous
trigger reset on every one-tick blip to 2-6 peers.

`vendor/iroh` now retries: a failed rebind arms `PendingRebind` (250 ms
doubling to 5 s, unbounded), each attempt rebinds only the transports still
closed and, on success, runs the relay check, DNS reset, re-STUN and QUIC
notification the link-change handler skipped. Witnessed with a deterministic
scratch node in its own netns on va: a duplicate of the iroh UDP fd is taken
with `pidfd_getfd`, a dummy link forces a major change (rebind fails
EADDRINUSE on the v4 socket), then the duplicate is closed. The pre-patch
binary was still unbound 9 s later; the patched binary retried at +0.25/+0.5/+1 s
and re-bound `0.0.0.0` 0.5 s after the release (`transport rebind recovered`).
The `meshwatch` small-fleet floor is a separate open row.

## 2026-09-20 — the litebox artifact GC hashed the whole cache on every publish, so a shim change took a node's apps dark for an hour

Rolling the `process.title` fix to fc-sanjose changed `runtime_source_sha256`
for every litebox app, so every app rebuilt its combined runtime archive at its
next cold start. Each rebuild ended in `gc_artifacts_locked`, which called
`verify_immutable_open` — a full SHA-256 — on the app archive and every runtime
archive of every reference, under the global `artifact_lock`: ~125 references x
~400 MB, measured as a new ~230 MB tar every 2 m 43 s in the runtimes directory.
A cold start therefore queued behind ~163 s of hashing per app ahead of it, and
a request that gave up (curl timeout, browser) left the queue, so
`just-survey-bot` / `survey-botbot` answered nothing for 60/90/100/200/420 s
probes across two binaries (rolling back re-invalidated the single-entry
per-key cache and rebuilt everything again).

The GC now checks that each referenced archive exists as a regular file and
leaves content verification to where the bytes are used
(`verify_immutable_open` at launch and at publication). Its blast-radius guards
(empty keep set, max reap fraction, grace age) are unchanged.

## 2026-09-19 — every Next.js app on a litebox node lost all its environment variables (`process.title` wipes `process.env`)

tokenhun.shadw.app (a Next app served from fc-phoenix) answered
`PROXY_API_KEY is not configured on the proxy server` although the variable was
set in the project, baked into the deployment, and present in the litebox
runner's own environment (`/proc/<pid>/environ`).

Reproduced and bisected on phoenix with real litebox guests. A guest given the
exact production environment saw all 13 variables; a plain `node -e` kept them;
the same app driven in-process or through a custom `http` server answered 401
(the middleware saw the key); only `next start` answered 500. Instrumenting the
merged tar showed the guest's `process.env` already at 5 keys when `listening`
fired, and the last step before it was `process.title = "next-server (v…)"` in
`startServer`. In a guest, `process.title = 'x'` takes `process.env` from 12
variables to 0: libuv's `uv_set_process_title` rewrites the argv block in
place and litebox's initial stack keeps the environment strings inside it.

Fixed in `litebox-bind-shim.js` (preloaded into every Node guest): `process.title`
becomes a JS-only accessor and never reaches the native setter. Changing the
shim changes `runtime_source_sha256`, so each deployment's combined runtime
archive is rebuilt at its next launch. Verified by replacing the shim in a
scratch guest's tar: the same `next start` went from 500 to 401 without the key
and past the middleware with it. Any Node app that assigns `process.title`
(PM2-style launchers, Next, Nest) was affected the same way.

## 2026-09-19 — apps 15 s slow on three of four edges: `connection: keep-alive` + a bodiless response wedged the tunnel's connect gate

`just-survey-bot.shadw.app` / `survey-botbot.shadw.app` (hosted on fc-sanjose)
took 15.1–15.4 s through va, va3 and phx — three of the four round-robin DNS
answers — and 0.1–0.6 s through sj itself. `x-hive-transport: http-direct`: the
edge forwards over the peer's iroh trunk first, got no first byte for
`HIVE_P2P_FIRSTBYTE_MS` (15 s), then the HTTP fallback answered in ~0.2 s.
tcpdump on sj showed nothing carrying that Host reaching `:8787` until the
fallback arrived; forwards to fc-phoenix over iroh took 0.25 s.

Cause (mine, `eb20c76`): `fluid_tunnel::server::proxy_local` now sends
`connection: keep-alive`, and for a response with neither `Content-Length` nor
chunked framing it reads until EOF. A HEAD response or a 1xx/204/304 has no body
by definition and no framing, so the read waited for a close a keep-alive
upstream never sends — while holding `connect_gate(local_http)`, whose permit
count is 1 on litebox nodes. One such exchange on sj sat idle for 59 minutes
(`ss` showed the loopback socket owned by hive-cloud's own fd, `lastsnd`
3.5 M ms) and every later peer-forwarded request queued behind it until the
peer's first-byte timeout. Destroying just that socket (`ss -K`) restored iroh
forwards immediately (15.2 s → 5.5 s → 0.46 s, then all four nodes 0.12–0.5 s
for both sites).

Fixed: `proxy_local` treats HEAD/1xx/204/304 as bodiless and ends the exchange
at the head. Also added a per-candidate iroh cooldown in `edge.rs`
(`HIVE_EDGE_IROH_COOLDOWN_MS`, 120 s): after a `PostSendTimeout` or
`DeadPeerTimeout` the candidate goes HTTP-first and one probe request retries
iroh, so a genuinely dead trunk costs one request 15 s instead of all of them.
The cooldown is compiled and deployed but its branch has not been exercised
live. The response that wedged sj was 433 bytes and was not captured, but the
mechanism is reproduced: on phx (old code) one `HEAD /` forwarded over iroh hung
and every following GET took 15.2 s (`http-direct`); after the fix, 6 HEADs per
edge to the sj-hosted app took 0.04–0.16 s, GETs stayed on `iroh-p2p` at
0.1–0.4 s, and no node held a stuck loopback exchange. HEAD is one trigger;
204/304 are the same code path and were not reproduced.

## 2026-09-18 — a managed Postgres was wiped and re-created every ~70 s because the leader's stale `simulated` copy overwrote the host's promotion

`surveybot-db` (project `survey-bot-real-2`, host fc-virginia-3, created
2026-09-09) sat at `provisioning`/`simulated` on the leader and every node.
`spawn_db_reconcile` retries such a record ON ITS HOST: it scrubs the same-name
container as an orphan (`podman rm -f -v`), provisions a fresh one, and promotes
the LOCAL record to `ready`/`live`. Nothing carries that promotion back to the
leader, and `DatabaseStore::merge_synced` lets the remote copy win every
collision, so the next `store=databases` sync (≤60 s) put `simulated` back and
the next reconcile tick destroyed the running database and started an empty one
over it: 283 container create/died/remove cycles in 6 h on fc-virginia-3. Each
cycle is also a host network-change event (see the iroh rebind finding in the
PRD), and the leader's `connection` map stayed empty, so no `DATABASE_URL` could
ever have been injected for it.

Fixed the loop: `merge_synced` never lets a `simulated` copy overwrite a local
`live` record with the same `created_ms` (the reconcile loop already assumed "no
code path demotes live→simulated"; this enforces it). Verified on fc-virginia-3:
after one last re-provision at restart the container stayed `Up 8 minutes` and
the record stayed `ready live` with 14 connection keys through repeated syncs;
before, a new container appeared every ~70 s (9 in 10 min). NOT fixed: the host
still does not report its promotion to the leader, so the leader, the UI and
env injection continue to see `provisioning` for this database.

## 2026-09-18 — GuardianDB could latch itself wedged when a caller awaiting a slow init was cancelled

Landed in `6ea49d7` alongside the ledger fix below (the commit tooling ignored
the path restriction, so it carries both; the binary running on fc-phoenix was
built before this change and contains only the ledger fix).

`guardian::handle()` parks a slow init's `JoinHandle` in `INIT_INFLIGHT` so the
next caller re-awaits the same attempt. The waiter `take()`s that handle and
re-parked it only on the timeout branch, so a caller dropped mid-await —
cancellation, not timeout — lost it: the slot read empty while the init kept
running and holding its redb lock, and the next caller started a second init,
hit `Database already open` on attempt 2 and latched the node wedged ("restart
hive-cloud to recover"). Witnessed on shadw3: the first init had been
re-awaited cleanly every 30 s for 25 minutes, then a second `opening iroh
client` began 11 s after one more caller took the handle and failed 2 ms later.

`InflightInit` now owns the handle while a caller awaits it and re-parks it on
drop unless the attempt finished, so timeout and cancellation are one path.
Compiled, not yet witnessed live on a node; why the first init sits at
`opening 'hive-state' KV store` for 25+ minutes on shadw3's 25 GB store is a
separate, open question.

## 2026-09-18 — fc-phoenix crash-looped 21,679 times on an intact deployment ledger

`DeploymentLedger::open` verified its checksum by re-serializing the decoded
`LedgerPayload`. The integrity-chain work added `DeploymentAcceptance.
integrity_chain` (`#[serde(default)]`, always serialized), so the 19 accepted
entries in fc-phoenix's ledger — written by the previous binary, without the
field — re-serialized with `"integrity_chain":[]` and hashed differently from
what had been stored. The roll on 2026-09-18 03:03 UTC reached phoenix and the
node panicked at `state.rs:623` on every boot from then on, restarting every
~3 s and serving nothing. The file itself was never damaged: its stored
checksum equals SHA-256 over the raw payload bytes exactly.

Fixed: the payload is now read as an undecoded `RawValue` and the checksum is
verified over those stored bytes; the decoded struct is only used after the
check passes, and the file is rewritten in the new shape on the first boot that
loads it. Verified by booting the fixed build against a copy of phoenix's real
ledger: it loads, all 19 entries survive with an empty `integrity_chain`, and
the rewritten file's checksum verifies over its raw bytes. A genuine mismatch
still fails closed.

## 2026-09-03 — npm's `$PATH` entry points were never staged into the guest; fixed, but litebox has no guest-side shebang execution at all

Follow-up to the same-day GNU-tar-header fix below: with `node -v` working
and the panic gone, `npm -v` as the first command in a fresh shell still
failed with the identical `Fatal error: glibc detected an invalid stdio
handle` a not-found command produces. Root cause was staging-completeness,
not the tar header: `walk_support_tree` staged every file UNDER
`/usr/lib/node_modules_22`, but `/usr/bin/npm` and `/usr/bin/npx` — the
`$PATH`-visible symlinks a typed `npm`/`npx` actually resolves through
(`PATH=/usr/bin:/bin` in the guest) — point INTO that tree and were never
staged themselves, so `PATH` lookup found nothing.

Fixed: `SupportTree::entry_points` names each `$PATH` symlink and its
expected real target; before staging, the host symlink is resolved with
`canonicalize` and compared against that expected target (never trusted
blindly — a base-image change that moved npm's real file would otherwise
stage a symlink to nothing), then emitted as a real symlink tar entry
(`tar::EntryType::Symlink`, litebox's reader already handles `SYMTYPE`).
Verified directly by copying a live shell's built tar to disk and reading
it back with Python's `tarfile`: `usr/bin/npm` is a symlink entry pointing
at the real staged file.

That still wasn't sufficient — `npm -v` continued to fail identically.
Isolated by comparing `node <path-to-npm-cli.js> -v` (works, prints the
real npm version, exit 0) against bare `npm -v` (still exit 134): **litebox
has no guest-side `execve` shebang-following logic at all** — confirmed by
code search across the entire litebox repo, zero hits for shebang/
interpreter handling. `npm-cli.js` begins `#!/usr/bin/node-22`; a real
Linux kernel re-execs the named interpreter on that line automatically,
and litebox's guest emulation simply doesn't implement that step. This is
a genuine, separate upstream gap (`litebox-guest-execve-no-shebang`) that
affects any shebang script a shell tries to run by name — `npm`, `npx`,
`pip`/`pip3` (also a shebang script on this host), and any user-authored
`#!/bin/sh`/`#!/usr/bin/env` script — not something more file-staging can
fix. The symlink fix ships anyway: it is correct, harmless, and is exactly
what makes `node <script>` and any future shebang-execution fix actually
reachable by name.

## 2026-09-03 — Staging npm/Python into sandbox shells exposed a second, un-fixed GNU-tar-header bug — fleet-wide shell panic, found and fixed same day

`node -v` inside a `node22` sandbox shell was a not-found (the shell tar
carried only `sh` + coreutils, never the sandbox's own runtime), fatal under
litebox's fork emulation. Fix: `ExecPtyRequest.programs` now names the
runtime interpreter, and litebox's `support_tree_for` stages `node`'s npm
tree (`/usr/lib/node_modules_22`, 15 MB / 1.7k files) or Python's stdlib
(`/usr/lib64/python3.12`, 63 MB / 2.8k files) alongside it — bounded, docs/
`__pycache__`/symlinks skipped.

That staging immediately broke every litebox shell fleet-wide once rolled:
`litebox/src/fs/tar_ro.rs:522` panicked (`ParseInt`, exit 101) the instant a
shell opened. Root cause was a REGRESSION only in the sense that nothing had
ever crossed its threshold before: `litebox_tar_header` built GNU-format tar
headers, and `runtime_artifact.rs`'s own 2026-08-26 fix for this exact bug
class ("never GNU headers", see `litebox-guest-tar-and-shim-gaps` memory)
had never been applied to this second, separate header constructor in the
same crate. The `tar` crate only takes the ustar prefix/name path-split
branch litebox's reader is written to expect when the header is genuinely
ustar; a GNU header over the 100-byte `name` field falls to the GNU longname
extension entry instead, which litebox misparses as a real file. npm's
deepest staged path (120 bytes) was the first thing in this crate's history
to cross that line. Fixed by switching `litebox_tar_header` to
`Header::new_ustar()`. Canary-verified with a direct binary swap on the
leader before the fleet roll: `node -v` works, a concurrent exec still works
(no regression to the same-day TUN-per-runner fix), the shell no longer
panics.

Not fixed, and unrelated to either change above: a litebox interactive shell
still runs at most one external command per session before going silent —
an upstream fork-emulation limitation, write-up drafted for the maintainer,
not filed pending the user's go-ahead (opening an issue on a third-party
repo is an outbound action).

## 2026-09-02 — Litebox sandbox shell: one TUN per cell made every second runner panic with EBUSY, and the shell tar had no `node`

The first thing typed into the newly-working dashboard terminal showed two
faults. `node -v` in a `node22` sandbox answered `Fatal error: glibc detected
an invalid stdio handle` plus `sh: tcsetattr: Inappropriate ioctl for
device`, and the shell was wedged afterwards (`id`, `exit` produced
nothing). And a runner panicked at `litebox_platform_linux_userland/src/
lib.rs:245 failed to set TUN interface flags: EBUSY`. Both reproduced
deterministically on the leader with a websocket driver (`shell-run.py`).

- **One TUN device per cell, one owner per TUN.** `setup_cell_net` created
  `lbt<i>` at provision and every runner of that cell — the function
  process, each `exec_command`, each `exec_pty` — was started with
  `--tun-device-name=lbt<i>`. litebox attaches with `TUNSETIFF`, which a
  second process cannot do on an attached device: the interactive shell held
  it, so a one-shot exec against the same sandbox (`POST …/commands`, the
  dashboard's Run panel) died at startup with exit 101, and a second shell
  did the same. Now `allocate_link()` hands every exec and shell runner its
  own device and `/30`; the armed `LiteboxLinkRollback` moves into the
  runner's waiter task and deletes the device when the runner exits (the
  guest-tar guard rule). The cell's provision-time link serves only the
  function process; `terminate` still kills every exec and shell group
  first, so their links go with them.
- **The shell tar staged sh + coreutils only.** The one-shot exec path
  stages the resolved program with its `ldd` closure, the pty path never
  did, so the sandbox's own runtime was absent from its shell — and under
  litebox's fork emulation a not-found is the worst case, not an error
  message. `ExecPtyRequest.programs` (additive, defaulted; Firecracker
  ignores it) carries the interpreter (`node` for `node*`, `python3` for
  `python*`) from `sandboxes_platform::open_shell`, and the litebox shell
  tar stages each resolvable one, warning about any it cannot resolve
  instead of refusing the shell.

Witnessed on the leader after the roll (exe `7bd1e9e057fc`): `node -v` in
the shell answers `v22.22.2`; a `POST …/commands node -v` while that shell
is open exits 0 with the same output (was exit 101, the EBUSY panic); two
shells opened together both prompt and run `id`; the per-runner `lbt` links
are gone once the runners exit (1 before, 0 after) and no runner survives
the deletes.

Still open, recorded: **a litebox interactive shell runs exactly ONE
external command per session** — measured `id; id; uname -a`: the first
`id` answers, the second prints nothing, and `uname`/`exit` never return
(builtins are unaffected). That is litebox's fork emulation on the parent
side (the child of the second `fork()` never reaches `exec`), the same
family as the fork-child faults already documented, and it is what wedged
the shell after the pre-fix `node -v` too. Upstream (the AnEntrypoint fork's
Task #51), tracked as `litebox-notfound-wedges-shell`. Also unstaged:
`npm`/`npx` (scripts over the host's `node_modules_22` tree,
`sandbox-shell-stage-npm`) and Python's stdlib
(`sandbox-shell-python-stdlib`).

The roll itself reached 10 hosts: between the previous roll and this one the
operator terminated nine more instances (sp, sj3, sj4, sj5, va4, va5,
cvmsj1, cvmsj2, fr; Tencent CloudAudit 22:08–22:27 UTC), so the live fleet
is bkk, hk, phx, seoul, sj, sj2, tokyo, va, va2, va3. Inventory, lockdown
roster and docs still describe 22 — PRD
`gpu-hosts-terminated-inventory-cleanup` (rescoped to the full set).

## 2026-09-02 — The dashboard terminal never connected in production: the session cookie was host-only behind the reverse proxy, so the api-host WebSocket arrived anonymous

Reported as `WebSocket connection to 'wss://api.shadw.cloud/v1/projects/
servermcserver/sandboxes/sbx_86001c0447a14171/shell?cols=138&rows=26'
failed:` — the browser's only diagnostic. The server side was healthy the
whole time: the sandbox was created at 19:06:33 UTC on fc-sanjose (litebox),
and a dashboard-shaped create under the same project, opened with the same
`hive_jwt` cookie a browser would send, upgraded 101 on the owner AND on the
round-robin non-owner (fc-sanjose-2, forwarded over the mesh). What differed
in the browser was that the cookie was never sent. `ui/app/api/token/route.ts`
derives the cookie's `Domain` (`.shadw.cloud`, so it also reaches
`api.shadw.cloud`) from the request's `Host` header — but hive-node's
`dashboard_proxy` forwards to the Next server on loopback with
`Host: 127.0.0.1:3002` and carries the real host ONLY as `x-forwarded-host`
(which `ui/proxy.ts` already reads for exactly this reason). `cookieDomain()`
therefore returned `undefined` in production on every node, every session
cookie was host-only for `shadw.cloud`, the `wss://api.` handshake went out
without it, `open_shell` saw an anonymous caller and refused 403 `project
belongs to a different team`, and nothing logged it: both refusals in
`open_shell` return before its audit line, so the leader's journal and audit
had a `create` and a `delete` for the sandbox and nothing in between.

Fixes:

- `cookieDomain()` reads `x-forwarded-host` first (then `host` for local
  `next dev`); the mint also expires the stale host-only cookie in the same
  response so an existing session does not keep authenticating the dashboard
  origin with the old (possibly other-tenant) cookie for its remaining hour.
- `terminal-panel.tsx`: a close before open now runs a diagnosis through the
  same-origin `/cloud` read of the sandbox (same tenant + record gate, minus
  the upgrade, WITH a status): 200 there means only the api-host hop refused,
  so the session is re-minted and the connect retried once; 404/401/403 or a
  transport failure is written into the pane and the badge instead of a
  silent "disconnected".
- `open_shell` logs every refusal at INFO with project, sandbox, status,
  anonymous flag, claimed tenant and the reason, before returning it.
- The dashboard fanout's two `ui/` rsyncs exclude `.gm` (gm-tooling state a
  watcher rewrites at will): the first deploy of this fix aborted on the
  leader with rsync rc 24, "file has vanished", when that directory churned
  mid-transfer, and the play stopped before any dashboard was built.

Not a browser-witnessed fix: no Clerk session is available to this
environment, so the proof is the server-side cookie-path upgrade (101 on both
API nodes), the proxy code that strips `Host`, and the token route's
derivation — deterministic, but the first real-browser confirmation is the
user's next terminal open after the dashboard deploy (a page load re-mints
the cookie with the parent domain).

Shipped: dashboard build `E6BjeBsrFeA65wb5dU7QY` verified on 22/22 hosts
(the compiled token-route chunk carries the forwarded-host read and the
`maxAge: 0` expiry); backend roll to 19 hosts (leader exe `de2b67dd05e4`),
after which an anonymous upgrade probe on the leader leaves `sandbox shell
upgrade refused before the handshake project=servermcserver
sandbox=sbx_86001c0447a14171 status=403 anonymous=true claimed_tenant=
reason=project belongs to a different team` in the journal, and the
post-roll terminal witnesses pass locally and through the public host. The
three hosts the backend roll could not reach (fc-sanjose-gpu-1/2/3) had been
terminated by the operator at 19:43 UTC that day (Tencent CloudAudit
`TerminateInstances`); their inventory/lockdown/docs cleanup is PRD
`gpu-hosts-terminated-inventory-cleanup`.

## 2026-09-02 — Tencent exposure ticket: two forgotten `http.server` hand-offs served a directory listing to the internet for a week; the fleet firewall's peer roster was 13 of 22

Tencent's scanner reported "Index directory traversal" on
`43.166.206.175:28126` and `:28127` (fc-virginia, ins-7d6a22bf) on
2026-08-27. Both were `python3 -m http.server 2812x --bind 0.0.0.0`
processes started on 2026-08-26 (11:12 and 12:43) by two-second
non-interactive root SSH sessions — an automated hand-off of freshly built
hive-cloud binaries between nodes (`/root/xfer4/v2/hc`, `/root/xfer5/lb/hc`,
plus a listener-less `/root/xfer3/s9/hc`; ELF builds, no key material) —
and left running. They sat inside TCP 20000-29999, which the platform
security group (`sg-gshd147x` in na-ashburn, `sg-2jhu3loa` in
na-siliconvalley) opens to `0.0.0.0/0` for published container ports, so the
listing was world-readable for a week and nothing on the node noticed.
Resolution: both processes killed and all three trees removed; closure
witnessed from a Tencent vantage (fc-frankfurt: connection refused) and a
non-Tencent one (fc-phoenix: timeout — Tencent's edge drops SYNs to closed
SG-open ports from outside sources, so an outside timeout never proves a
filter; probe from inside the provider). A 22-node sweep found no other
ad-hoc server.

What the same pass found and fixed alongside:

- **`scripts/hive-lockdown.sh` was enforcing a 13-host peer roster on every
  node** (21 via its iptables chain, fc-phoenix via nft). The nine hosts
  added since the last full prerequisites run (fr, phx, tokyo, seoul, va4,
  va5, sj3-5) were strangers to every other node's 8787/50052/50100-50999,
  which silently dropped their HTTP admin dispatch onto the iroh path.
  `gen-hive-lockdown.sh` regenerated the line to 22, the three lockdown
  tasks in `roles/prerequisites` now carry a `lockdown` tag so a roster
  change rolls alone (`ansible-playbook playbooks/site.yml --tags lockdown`,
  22/22 ok), and fc-virginia's stale second copy (a 5-peer nft table beside
  its 13-peer iptables chain, stricter than the live one) was deleted.
  Witness: fc-frankfurt and fc-phoenix now connect to va/sj/bkk:8787 where
  they timed out before.
- **An nft `socket cgroupv2` gate on the published range was tried and
  rejected by measurement**: on fc-virginia (6.12.33-pvm+, nft 1.1.1) it
  accepted 2 of 37 SYNs to hive-cloud's own `:20008` and dropped the rest —
  a listener SYN has no socket attached when the match runs. It ran in a
  throwaway table on a spare port and was removed; the record is in
  AGENTS.md so nobody re-tries it on a live port.
- **`crates/hive-cloud/src/listener_audit.rs` (new, every node, every
  `HIVE_LISTENER_AUDIT_SECS` = 300 s):** reads `/proc/net/tcp{,6}`, keeps
  wildcard LISTEN sockets inside the audited range
  (`HIVE_LISTENER_AUDIT_PORTS`, default 20000-29999), drops the ones this
  process owns (socket inode against `/proc/self/fd`, never a port list),
  resolves the rest to pid/cmdline/cgroup, WARNs every pass while they
  exist, and on the control-plane leader opens ONE deduplicated Major
  incident per (port, pid). Detection only — the platform never kills a
  process it did not start. `GET /v1/host/listeners` (operator, node-local
  like `/v1/dns/stats`) serves the last report; `supported: false` on a
  host without procfs means "not audited", never "clean". Witnessed on the
  leader after the roll (exe `f7b353c97be7`): an empty-directory
  `python3 -m http.server 28999 --bind 0.0.0.0` in a root session scope was
  reported 171 s later — WARN with port/pid/cmd/cgroup, `incidents opened
  opened=1`, the report naming `own_in_range: 4` (the raw proxy) and the one
  foreign entry, and a Major `Foreign public listener on fc-sanjose: :28999
  pid 3509642 (python3 -m http.server 28999)` incident on `/v1/incidents`
  (deleted after the test). A pass costs ~11.6 s of `spawn_blocking` on the
  leader (the `/proc/*/fd` walk resolving the owner); the incident stays
  open until an operator resolves it — auto-resolve on a clean pass is PRD
  `listener-audit-auto-resolve-incidents`.
- **`scripts/audit-public-listeners.sh` (new):** the fleet view — every
  wildcard TCP listener per node that is not a platform daemon, plus the
  lockdown branch and peer count each node actually enforces; exits 1 on
  any finding so it can gate a roll. Its first run flagged 34 `next-server`
  processes on fc-sanjose-cvm-2 and 10 on fc-sanjose bound to `*:3xxxx-4xxxx`:
  every one an orphaned host-spawned mock cell from before a hive-node
  restart (`KillMode=process` keeps them alive, nothing adopts or reaps
  them), cwd on a deleted checkout, zero connections, 5.4 GB RSS on cvm-2.
  Reaped by hand (34 on cvm-2, the older-than-hive-cloud ones on sj);
  the structural fix (a host-cell pid ledger reaped at boot, loopback bind
  for host spawns) is PRD `mock-cells-orphaned-on-restart` /
  `sec-host-spawned-functions-wildcard-bind`.
- **Security-group audit (read-only, `DescribeSecurityGroupPolicies` across
  eight regions):** bkk, hk, tokyo, seoul and sp each carry an ALL-ports
  `0.0.0.0/0` group beside the platform group, so the host lockdown is the
  only guard on those five; the platform group itself opens 8786/8787,
  5432/6379, 9000-9001 and 53 world-wide. Recorded as PRD
  `sec-sg-all-ports-open-groups` for an operator decision — an SG mistake
  locks out SSH, so it is not automated here.

Rules that fall out (AGENTS.md "Host firewall & public listeners"): never
bind an ad-hoc server to `0.0.0.0` on a fleet host (`scp`/`rsync`, or
`127.0.0.1` behind an ssh tunnel); run the audit script after any
hand-provisioning session and before a roll; the lockdown roster is
generated, never typed.

## 2026-09-02 — The relational mirror was dead fleet-wide: the index walk ran on every request path and never once completed

On every node — the leader, nine followers, nodes 71 s after boot — every
admin SQL query took exactly 10.00 s and answered `relation "X" does not
exist` for EVERY table, `pg_catalog.pg_class` counted 0 and
`information_schema.tables` was empty, while `GET /v1/admin/sql/tables` kept
listing all 16 (`known_tables()` is a hardcoded list). The mirror loop failed
the same way (`sync_teams upsert failed … relation "teams" does not exist`,
`ALTER TABLE project_teams add deleted_ms failed`), and zero relational
writes had succeeded since boot on the leader or on fc-virginia. Not a
missing DDL statement, not a schema-qualification mismatch: guardian-db's SQL
catalog is one document in the document-store index, and that index was
EMPTY. `relational::session()` ran the full namespace walk
(`storage().refresh()` → `get_many` over the whole `hive` namespace, on
guardian stores of 7.6 GB on sj and 57 GB on va) under `SQL_OP_TIMEOUT` =
10 s before EVERY operation, so the walk was cancelled every time, never
completed even once per process, the catalog document was never known, and
every `Session` loaded `Catalog::new()`. Worse, `CREATE TABLE IF NOT EXISTS`
against that empty catalog "succeeds" and persists a fresh catalog over the
real one — the whole-document read-modify-write `ensure_table_exists`'s doc
already warns about.

Fix (`crates/hive-cloud/src/relational.rs`, `vendor/guardian-db`): the walk
runs in ONE background task (`spawn_index_refresher`) with a bound sized to
the store (`HIVE_RELATIONAL_INDEX_BUILD_SECS`, 900 s) and re-walks on a timer
(`HIVE_RELATIONAL_REFRESH_SECS`, 120 s) behind the document store's live
sync; completing once flips `INDEX_READY` and logs `relational: index built`
with the elapsed time and key count. `session()` waits up to 10 s for
readiness and never walks. `init_schema`, `ensure_table_exists` and
`backfill_billing_normalize` refuse DDL until the index is real;
`run_readonly_query` answers an explicit not-ready error instead of "does not
exist"; `team_for_project` requires a walk completed within two intervals.
Vendored: `refresh_doc_index` builds the replacement key set off the lock and
swaps it in atomically (`DocumentStoreIndex::replace_hash_only`, keeping
fetched values whose hash is unchanged), so a completed refresh is never
observable half-built and a cancelled one leaves the previous index in
service; `GuardianRelationalStorage::index_len` exposes the key count. Verify
on a rolled node: `pg_class` ≥ 16 and a sub-second `SELECT count(*) FROM
project_teams`; an unrolled node stays at 0 / 10.00 s. If the walk cannot
complete inside its bound on a node, the journal now says so once per
attempt (`index walk exceeded its bound`) and nothing writes the catalog.

The first canary of that fix found the second half. The index built in
69 ms on fc-frankfurt and 40 ms on fc-virginia (3,783 keys — the walk was
never slow; the per-session refreshes had been cancelling each other), the
refresh timeouts went from 56,000 per boot to zero, and the mirror's schema
reconcile still logged `relation "project_teams" does not exist` ten times
in the same millisecond. `GuardianRelationalStorage::{get, scan}` read
through `index().get_bytes`, a cache-only lookup that answers `None` for
every key a cold-start walk registered by hash and nothing has read yet; the
catalog is exactly such a key on every boot, so the engine loaded an empty
catalog on a fully built index. Both now fall through to the store's async
`get`, which fetches the value lazily and caches it. And `init_schema`, the
only path that creates the catalog and tables, was a single boot-time
attempt that returned silently when GuardianDB was not open yet and never
ran again; it retries every 15 s within the index-build bound and logs
`relational: schema bring-up complete` with the key count. Note for
anyone probing a follower: `POST /v1/admin/sql/query` is a mutation-shaped
request and the admin ingress forwards it to the control-plane leader, so a
follower's own relational state is witnessed from its journal (the
bring-up line, zero `does not exist`), not from its admin SQL answer.

The third canary found the last piece. Bring-up now ran on both nodes
(index built in 14–23 ms, zero refresh timeouts, zero `does not exist`) and
two tables per node — `teams`, `billing_ledger` — still reported unverified:
`ensure_table_exists` verified with `SELECT 1 FROM t LIMIT 1`, which scans
every row, and on a cold index each remotely written row is a peer fetch
of hundreds of milliseconds, so the scan blew the 10 s statement budget
while the CREATE itself had succeeded. Verification now reads
`information_schema.tables`, synthesized from the catalog document with no
row access; after the walk flips readiness the refresher warms every row
value in one pass through the store's lazy `query`
(`GuardianRelationalStorage::warm_values`, logged with count and
duration), and nothing waits on it; the bring-up summary counts the tables
it gave up on and logs at ERROR when that count is non-zero instead of
claiming every table verified. Measured on the fourth canary
(fc-virginia): index built in 14 ms, bring-up complete with every table
verified in the catalog and zero errors within a second of start, and the
warm pass materialized 3,759 documents in 416.9 s — about 111 ms per row,
the peer-fetch cost the old per-row verification was paying inside a 10 s
budget — so a cold node's mirror is fully warm seven minutes after boot and
its catalog and schema are correct from the first second. The control-plane
leader after the fleet roll: index built in 4 ms, bring-up complete with
every table verified, `pg_class` = 28 and real row counts in milliseconds
where it answered 0 and "does not exist" after 10 s an hour earlier; its
warm pass took 1,607 s for 1,935 documents (about 830 ms per row on the
busiest node) and once it landed the statement timeouts stopped entirely.
Expect the `ALTER TABLE project_teams` reconcile to retry with a 10 s
timeout on every mirror tick until the warm pass ends; it converges on its
own and is not the pre-fix "namespace wedged" class its message still names.

## 2026-09-02 — hive-node never exited inside systemd's stop timeout: a post-barrier persist() parked the tokio driver

Measured over 75 fleet stops: `hive-node` took 78.4 s (+78.3..78.6 s) to exit
on its happy path, was SIGKILLed on 34 of the 56 stops on 90 s nodes, and on
the TencentOS hosts (bkk, hk, cvmsj1/2, gpusj1-3) was SIGKILLed at +5 s, before
the listener drain ended and before the platform-state flush ran. The graceful
sequence in `main.rs` (SIGTERM → 15 s listener drain → runtime-artifact drain
→ `persist::flush_blocking` → `mark_clean_exit` → `guardian::shutdown` →
`ep.close()` → exit) spent 15 s in an unconditional drain sleep with zero
connections open and 60 s in `guardian::shutdown`'s first wait, which never
succeeded: the GuardianDB writer commits one generation per 40–75 s. The
SIGKILLs had one cause. After `flush_blocking` closed persistence admission,
any later `persist()` (185 call sites; usually `main.rs`'s follower-sync
adopt right after "store follower-sync: adopted the leader's snapshot", the
metrics loop, or a mirrored gossip write) reached `admit_generation`'s
condvar wait — a std wait on a tokio worker thread, by design "until process
exit". When that worker was the one holding tokio's IO/timer driver no other
worker re-took it, every timer and socket on the runtime stopped (thread
snapshots: 0 in epoll, all 131 in futex, 0 CPU, one worker on a foreign
futex, journal silent for 74 s), the 60 s guardian timeout and the 90 s
in-runtime hard deadline stopped with them, and systemd SIGKILLed at 90 s.
Two smaller defects rode along: `restart_audit::spawn`'s 20 s heartbeat
rewrote the marker with `clean_exit=false` after `mark_clean_exit` had
stamped it at +15 s, so every self-exiting restart was reported UNCLEAN; and
the TencentOS hosts inherit `DefaultTimeoutStopSec=5s` from
`/etc/systemd/system.conf` because the unit template set no `TimeoutStopSec`.

Five fixes. `persist::admit_generation` returns `None` once admission is
closed and never waits — `persist()` is a no-op and `persist_durable()`
returns `false` (fail closed), with one WARN per process. The hard deadline
is a `std::thread` sleeping on the OS clock (`hive-shutdown-deadline`),
default `HIVE_SHUTDOWN_DEADLINE_SECS` 90 → 75 so it stays under systemd's
stop timeout. The listener drain polls `Handle::connection_count()` every
250 ms and ends as soon as it reads zero instead of sleeping the whole 15 s
grace. `HIVE_GUARDIAN_SHUTDOWN_TIMEOUT_MS` defaults to 10 s for each of the
three sequential waits. `mark_clean_exit` latches an `AtomicBool` the
heartbeat checks before every marker write. `hive-node.service.j2` sets
`TimeoutStopSec=90s` explicitly. Expected graceful path after the roll:
~30 s, exiting on its own before either deadline.

A sixth, found while canarying the five: on fc-virginia the old binary was
SIGKILLed 1.2 s after SIGTERM, not at 90 s, and the journal explained it —
`hive-node.service: A process of this unit has been killed by the OOM
killer`, twice a second apart. The unit's cgroup holds every child hive-cloud
spawns (tenant builds, conmon, litebox runners, firecracker) under the
node's memguard `MemoryMax` (31.5 GiB on va), and systemd's default
`OOMPolicy=stop` turns ONE OOM-killed child into a stop of the whole node —
the SIGTERM hive-cloud logged right after the OOM line proves today's victim
was a child, not hive-cloud (12 such OOM-stop events in 24 h on va). The
cgroup's 35 lifetime kernel OOM kills are a different case: every one was
hive-cloud itself at 26.8 GB anon-rss on 2026-08-28, the KV balloon fixed
since. `hive-node.service.j2` now sets `OOMPolicy=continue` — a child's own
failure path reports it and the control plane stays up; hive-cloud itself
being the victim still exits and restarts under `Restart=always` exactly as
before.

## 2026-09-02 — Leader relay drops: iroh's relay actor read its stream only when it had nothing to send

The control-plane leader logged `iroh::socket::transports::relay::actor:
Dropping received relay packet: no available capacity` 458,427 times in 24 h
(journald suppressed 6.0 M more), 227,635 in the worst hour and 6,932 in the
worst second, while followers logged 227 (fr) and 0 (va). The line is NOT
the embedded relay server (which has zero clients on every node): it is
iroh's client-side `ActiveRelayActor` inside hive-cloud. Both of its loops
are `select! { biased; … }` and polled the outbound queue before
`client_stream.next()`, so the one node that SENDS on relay paths at volume
— the leader, serving wholesale store_sync pulls, rosters and gossip to its
relay-only peers — never read its relay stream while it had something to
send. The relay server's 2 s write timeout then reset it (303 `Connection
reset by peer` on the leader against 47 and 35 on followers), and the
released backlog decoded in one burst (2,000–4,500 frames inside 100 ms,
inter-drop gaps under 50 µs) into the per-endpoint 512-slot receive channel
the QUIC driver drains a batch at a time. Vendored patch
(`vendor/iroh/CHANGES.md`, "read before send"): the read arm precedes the send
arm in both selects (`biased` kept: stop, priority inbox and timeouts stay
first; a received message is a non-blocking `try_send`, so reads cannot
starve sends), and the channel is 4096 deep. Unchanged upstream in 1.1.0.
Rolled as a canary to fr and va with a `/v1/mesh` assertion before the fleet.
Open beside it (PRD `leader-udp-11204-rebind-addrinuse`): the leader's pinned
iroh UDP socket fails to rebind with `AddrInUse` a few times an hour and stays
closed for minutes; a 12-minute holder watch ruled out a persistent foreign
holder, the in-process candidate is still unidentified.

## 2026-09-01 — Sandbox terminals no longer disconnect instantly: the pty runner's guest tar outlived by its waiter

Every dashboard sandbox terminal closed the moment it opened. Reproduced
against the leader's own listener and through the public round-robin host:
the websocket upgrade succeeded (101) and the server immediately sent
`Error: No such file or directory (os error 2)` followed by
`{"type":"exited","exit_code":1}`. Standalone, the same runner with the same
tar runs an interactive shell on a pty fine, so the defect was in how
`LiteboxBackend::exec_pty` spawned it: it built the shell tar and its
descriptor-bound alias (`/proc/self/fd/N/initial-files.tar`), spawned the
runner, and returned — dropping both guards at the end of the function.
Their Drop impls unlink the alias name and its scratch directory, and the
runner's own open of `--initial-files` a few milliseconds later found nothing
(the inherited directory fd still existed; the entry did not). `exec_command`
moves both guards into its waiter task and drops them after the runner exits,
which is why one-shot commands worked and shells never did.
`crates/hive-backend/src/litebox.rs`: the pty waiter now owns both guards
exactly like the exec waiter.

With that fixed the shell died a second way, with no output and status 21.
Litebox has no job-control tty ioctls (`tcsetpgrp` answers ENOSYS, -38): a
shell whose stderr IS the pty goes interactive by itself, enters job-control
setup, and the runner exits `exit_group(277)` before a prompt byte —
measured with the staged tar for `/bin/sh`, `+m`, `--norc +m` and `-i +m`
alike, while `/bin/sh -i` with stderr off the pty prints its prompt, runs
input and exits cleanly, warning once "cannot set terminal process group
(-38)" / "no job control in this shell". `exec_pty` now keeps stdin/stdout on
the pty, gives the runner a stderr pipe pumped into the same terminal stream
(those two one-line warnings filtered), and passes `-i`. Job control inside
a sandbox shell stays a litebox gap (PRD `litebox-guest-exec-pty-support`).
The pump forwards raw chunks with LF normalized to CRLF, never whole lines:
an interactive shell writes its prompt to stderr with no trailing newline,
and a line reader held it forever (the roll-5 cut: 101 upgrade, runner
alive, zero frames). Measured on that cut before the chunked pump rolled,
with a websocket client that typed without waiting for a prompt: `echo`,
`id` (`uid=1000`) and `exit` (`{"exit_code":0,"type":"exited"}`) all
round-tripped within 100 ms, and the four held prompts arrived as ONE frame
(`sh-5.2$ sh-5.2$ sh-5.2$ sh-5.2$ exit`) only when `exit` finally supplied
the newline — the terminal was interactive but blank, which is the whole
defect the chunked pump removes.

With the chunked pump live (witnessed green on the leader's listener AND
through `https://api.shadw.cloud`: 101, prompt, `TERM_OK_42`,
`{"exit_code":0,"type":"exited"}`) two smaller defects showed in the frames
and are fixed in the same file:

- The first frame was a bare `\r\n`: the pump swallowed the CRLF BEFORE a
  dropped job-control warning, so the newline after the LAST warning leaked
  and every terminal opened with a blank line above its prompt. A dropped
  line now owns the newline that follows it.
- The next prompt overtook the previous command's output (`sh-5.2$ echo
  …\r\nsh-5.2$ TERM_OK_42`): stderr (pipe) and stdout (pty) were two
  channels read by two tasks. The shell tar now stages `/usr/share/hive/shellrc`
  (`exec 2>&1`) and the runner is spawned with `ENV` naming it — the shell
  still STARTS with stderr off the pty (the arrangement that keeps the
  runner alive through job-control init) and then moves it onto the pty
  itself, so prompt, output and command stderr are one ordered stream with
  the pty's own CRLF translation. Linux-gated code, so `cargo check` ran on
  fc-virginia, not only locally.
- Rolled, that cut fixed both (first frame `sh-5.2$ `, then `TERM_OK_42`,
  then the next prompt, on one channel) and failed at `exit` with runner
  exit 101: `thread 'main' panicked at litebox/src/fs/layered.rs:683:67:
  called Result::unwrap() on an Err value: NoWritePerms`. The rc file's tar
  entry had implicitly created `/root` in the guest, not writable by the
  guest uid, and the interactive shell's history write at exit hit a
  permission check litebox unwraps instead of answering EACCES — where a
  MISSING `/root` (every earlier cut) failed that write silently. The rc now
  lives at `/usr/share/hive/shellrc`, nothing under `$HOME`, and `HISTFILE`
  is cleared so the shell never writes history. The panic itself is a
  litebox defect (PRD `litebox-layered-fs-nowriteperms-panic`).
- Final witness on the control-plane leader after the fleet roll
  (2026-09-02, exe `6c64268dd375`), on its own listener and through
  `https://api.shadw.cloud`: 101, first frame `sh-5.2$ `, `TERM_OK_42`
  echoed, `exit` → `{"exit_code":0,"type":"exited"}`, VERDICT PASS both
  ways; the probe's frames read prompt, output, prompt in order, and `id`
  answers `uid=1000`. What stays open is litebox's own job control (no
  `tcsetpgrp`), which the shell reports once at startup and the pump drops.

## 2026-09-01 — Deploy dispatch falls back past an unreachable node; peers keep their real home relay; a missing main entry fails the BUILD, not the node

Two production failures reported together, root-caused as two different
projects (`crates/hive-cloud/src/git.rs`, `schedule.rs`, `gossip.rs`;
AGENTS.md "Deploys"):

- **`express` (regions frankfurt, tokyo): `Placement → fc-frankfurt`,
  `deploy dispatch failed — iroh: no reply`, `Could not reach any target`.**
  Eight consecutive builds over 75 minutes failed the same way, each in
  5–15 s (the dial failed; the 20 s budget never elapsed), while ten capable
  nodes sat idle: the leader had marked fc-tokyo unhealthy, so `place()`
  returned ONE candidate and the coordinator gave up after it. Only three of
  the eight fell inside any restart window. fr builds fine (3/3 ready in
  48 h) — "builds failing on frankfurt" were dispatches that never arrived.
  The leader↔fr iroh pair is chronically sick for a proven reason:
  `gossip::relay_hinted_addr` replaced fr's real home relay
  (`https://fc-virginia.relay.shadw.app:3343/`, carried in its `iroh_addr`)
  with its embedded `http://162.62.83.144:3341`, and every Tencent `:3341` is
  a TCP timeout from every vantage (security group) — so a cached-hint dial
  had exactly one path, direct UDP, and when that flapped it timed out at 5 s
  and fell into discovery (1,014 such timeouts in 24 h).
  - `schedule::dispatch_fallbacks`: an ordered list of capable + reachable
    remote candidates (configured regions first in request order, then
    distance/load/free disk/name), excluding tried targets and self.
  - `git::run_build`'s pure-remote branch walks that list when nothing ran
    (`HIVE_DEPLOY_DISPATCH_FALLBACK_MAX`, default 3; each attempt bounded by
    the existing 15 s HTTP + 2×20 s iroh budgets), logs
    `→ fallback i/n: <node> (<region>) — <unreachable> could not be reached…`,
    stops when any node ran, keeps a container-lease holder sacrosanct, and
    ends with `⚠ Deployed on X because Y could not be reached`. The iroh
    failure text now carries measured elapsed ms and distinguishes
    dial-failed from budget-elapsed. `Could not reach any target` is only
    ever printed after the whole list is exhausted.
  - `relay_hinted_addr` keeps the peer's own home relay when its address
    carries one and steers only relay-less addrs. This is a dial-side mesh
    change and is rolled canary-first (`--limit fr,va`) with a `/v1/mesh`
    assertion before fan-out, per the retain_dialable lesson.
  - Characterized, not fixed (PRD `leader-relay-actor-saturation-transport-wedge`):
    the leader logged 459,062 `Dropping received relay packet: no available
    capacity` in 24 h and the transport-wedge signature 60×, through two
    restarts.
- **`shoomoo2` (github fatbearsk/serverless-clawdbot@xstate) on
  fc-sanjose-3: `main entry "/workspace/server.js" is missing from the
  immutable application archive`.** The repository has no `server.js` (a
  Next.js 16 app; `scripts.build = "next build"`), and a `fluid.json` lane
  runs ZERO install/build commands (`produce_manifest` returns
  `Manifest::from_json` untouched), so `start_cmd: ["node","server.js"]`
  can never have worked. The platform defect was how it surfaced: at the
  post-registration readiness launch, as `NodeBackendUnavailable` — a NODE
  fault charged to the pool for a tenant file that does not exist.
  - `git.rs` `preflight_direct_entries`: before sealing, every Node/Bun
    function whose argv is a plain `node <entry>` / `bun <entry>` must have
    that entry in the checkout; the refusal names the function, the entry,
    the exact path examined, the sealed root's listing, package.json
    `scripts.start`/`scripts.build`/`main`, the deciding field
    (`fluid.json functions[N].start_cmd` vs build-derived), the lane rule and
    the three fixes. Verified not to false-positive on the platform's own
    launch shapes (FDI Next dist-bin, SvelteKit `node build`, exported-app
    launcher, Build Output v3 launcher, `bun run --bun start`, `npm start`).
  - `litebox.rs` `validate_archive_main_entry`: a `Not found in archive` tar
    miss is `DEPLOYMENT_START_FAILED` with a message naming the entry and the
    fields, never `NODE_BACKEND_UNAVAILABLE`; other tar failures keep the
    launch refusal.
  - The tenant fix is on the user's side: remove `fluid.json` (framework
    detection then runs install + `next build` and derives the start command
    from `scripts.start`) or commit a self-contained server. Honoring
    install/build commands inside the fluid.json lane is PRD
    `fluid-json-lane-honor-build-commands`.

Live witnesses after the roll (canary `--limit fr,va`, then all 22 hosts, both
glibc lanes; fr's first 45 s on the new binary showed zero cached-hint dial
timeouts against ten in a comparable span before):

- `POST /v1/projects/express/redeploy` on the new leader → `dpl-cf08186c74`:
  `Placement: region-aware scheduler → fc-frankfurt, fc-tokyo`,
  `→ fc-frankfurt: dispatching deploy (via iroh)`, `[fc-frankfurt] Running
  build`, `Sealed runtime artifact 1023774e301e`, `✓ fc-frankfurt: deployment
  ready` (tokyo likewise), state `ready`, aliased to express.shadw.app. The
  leader opened a trunk to fr within seconds of booting and logged 2
  cached-hint timeouts in 3 minutes where it had logged 45 per 5 minutes.
  The fallback walk was not needed on that run and is therefore not yet
  exercised live.
- The same repo and branch deployed fresh under a throwaway project
  (`gm-preflight-witness`, deleted afterwards) → `dpl-0f4ab200cf` state
  `error` with `Launch preflight failed for function "web" (functions[0]):
  main entry "server.js" — chosen by fluid.json functions[0].start_cmd =
  ["node", "server.js"] — does not exist in the deployment tree about to be
  sealed … package.json declares scripts.start = "next start", scripts.build
  = "next build". A fluid.json deployment ships the repository checkout
  AS-IS …` and no `Sealed runtime artifact`, `NodeBackendUnavailable` or
  `tar:` line.

## 2026-09-01 — Sandbox execs get a supervisor (deadline, kill, terminate sweep); litebox fork-child corruption root-caused

The first live proof after the fleet roll below created a real Litebox sandbox
on the leader (`owner_node: fc-sanjose`, `provider: platform`, no "simulated"
note — the thing the whole day was about) and then hung on its first command:
`POST …/commands` timed out at 60 s with zero bytes, `DELETE` answered 200,
and the `litebox-runner` for that sandbox was still alive seven minutes later
at 99.9% CPU until killed by hand.

Root cause, established standalone with the exact staged tar rather than
inferred (`crates/hive-backend/src/litebox.rs`, `SHELL_GUEST_OPTIONAL_PROGRAMS`
doc; AGENTS.md "Litebox"): the litebox `fork()` emulation gives a child whose
glibc state still points into the parent's mapping. Fork + immediate `exec`
works; a pipeline/subshell child aborts (`malloc(): unaligned tcache chunk
detected`), a child that touches stdio aborts (`glibc detected an invalid
stdio handle`), and two consecutive not-found commands make the second child
spin forever. The repro was handed to the litebox fork's owning session
(their Task #51); nothing here changes fork semantics.

What changed on this side:

- `sandboxes_platform::run_command` no longer drains the exec inline. A new
  `supervise_exec` task drains BOTH blocking and detached runs, so a dropped
  request future (client timeout, closed tab, the leader→owner forward's
  budget) can no longer orphan a running guest with its record stuck at
  `running`. It enforces a deadline — `HIVE_SANDBOX_RUN_MS` − 10 s for
  blocking runs (one knob with the forward budget, owner finalizes first), the
  sandbox's `timeout_ms` capped by `HIVE_SANDBOX_EXEC_MAX_MS` (1 h) for
  detached — and on expiry kills the guest process group via `kill_exec`,
  gives the waiter 3 s to report `ExecDone`, then closes the record `killed`
  with `[hive] command exceeded its N ms deadline …` on its stderr.
- `LiteboxBackend::terminate` kills every live exec and interactive-shell
  process group of the cell before tearing down its TUN; `exec_pty` runners
  now get their own process group and are tracked in `execs` under the
  session id, so a shell dies with its sandbox too.
- `SHELL_GUEST_OPTIONAL_PROGRAMS`: `uname`, `id`, `sed`, `awk`, `sort`,
  `xargs`, `tar`, … staged when the host has them (absent = skipped, never an
  exec failure). Under the fork defect a not-found command is the WORST case,
  so this is hang-avoidance as much as convenience.
- The `LiteboxExec` doc no longer claims guest forks are host forks: on the
  fleet's runner build the "child" is a second thread of the runner (gdb,
  `pgrep -P` empty); the group kill stays correct either way.

Fleet state this entry ships into: the roll below completed
`BACKEND VERIFIED: 22/22`; the leader fc-sanjose restarted as
`isolation backend: Litebox` with the 27-id trust list loaded and its 218
tenant containers surviving the restart (`KillMode=process`).

## 2026-09-01 — Sandboxes fail closed and route to a real owner; Linux mock nodes become Litebox

Commit `59357cdac9` carries ALL of this under a subject that names only its
Tencent hunk — the `git_commit` verb ignored its `paths` argument and staged
the whole tree (PRD row `gm-git-commit-ignores-paths`). This entry is the
honest description of that commit. It is the replay of local `138815cd20`
onto `dff7bf99c3` (cross-node terminal forwarding over the mesh,
`mesh_shell.rs`); the merge keeps that forwarding and points it at
`SandboxRecord::owner_node` — a non-owner node now tunnels the interactive
shell to the real owner over the existing `RawTarget` mesh surface and sends
`wrong_node` only when no mesh path to the owner exists.

- **Sandbox creation never returns a simulated success again.**
  `PlatformSandboxProvider::create_sandbox` fails CLOSED: a provisioning
  error is a typed `EngineUnavailable` naming the node and its backend, no
  record is persisted, and a Drop guard releases the half-provisioned cell.
  The previous path stored `status=failed` + note and returned `Ok` — the
  "this node has no real isolation backend … sandboxes are simulated here"
  records (e.g. `sbx_52b6a2da81324dc0`) came from the control-plane leader
  itself, which runs `MockBackend`.
- **The leader is a router, not a fallback provisioner.** `POST
  …/sandboxes` mints the id, then delegates to healthy exec-capable peers
  (`backend ∈ {firecracker, litebox}`, same region first, name order) with
  bounded per-peer and total budgets over the existing owner-hop transport
  (HTTP admin, else the gossip `POST` arm); exhaustion is a typed 503
  `SANDBOX_NO_CAPABLE_NODE` listing every candidate tried. Records carry
  `owner_node`; stop/delete/run/kill ride one typed owner RPC
  (`/v1/internal/sandboxes/owner-op`) that re-derives authority on the owner
  and never re-proxies; reads proxy to the owner. Internal hops require the
  fleet token or a tenant-bound `mesh-internal` JWT; `/v1/internal/sandboxes/`
  is exempt from both leader gates.
- **`LiteboxBackend` can run sandbox commands.** `exec_command`/`kill_exec`
  implemented on the same guest mechanism as `exec_pty` (fresh runner on the
  cell's TUN, guest tar with the program's `ldd` closure, separate
  stdout/stderr `ExecOutput` streams, exactly one `ExecDone`, `killpg` by exec
  id).
- **The pinned AnEntrypoint Litebox runner now works on RHEL-family hosts.**
  `roles/litebox/files/networking.patch` gained a third hunk: uid 0 gets
  `CAP_DAC_OVERRIDE` semantics in the guest in-memory FS. Without it the
  runner panicked at `lib.rs:293 NoWritePerms` because `/usr/bin` is 0555.
  With it `hive-cloud --litebox-probe` PASSES on fc-tokyo, fc-seoul,
  fc-virginia-4/5 (runner `f970bfe70ac86d4e`, byte-identical),
  fc-sanjose-cvm-1/2 (`cb20b4e9f3cf03f5`) and fc-sanjose; the first six run
  `backend=litebox` in production, the leader flips on its next restart.
  Details in `PATCHES.md`; `hosts.ini.example` documents the `[litebox]` group
  and the per-host `litebox_verified` mark; the AGENTS.md Litebox section is
  drained to pointers.
- **Tencent dedicated IPv4.** `AllocateAddresses` tags use `{Key, Value}`;
  `TagKey`/`TagValue` is the filter type and was rejected with
  `UnknownParameter: Tags.0.TagKey` before any address was purchased.

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
