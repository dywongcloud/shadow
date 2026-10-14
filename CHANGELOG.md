# Changelog

## 2026-10-14 — the token mint is limited per user with a peer-wide backstop, and only guesses at the internal token spend the strict per-peer budget (0f4187d92b)

Every session of the fleet reaches `POST /v1/token` through ONE dashboard server, so the single per-peer budget (20 per minute)
was shared by every user: about 300 org sessions (each re-mints every 15 minutes) saturated it, and a stream of bad guesses from
the dashboard's own peer address spent the same budget, so legitimate mints were refused too. `mint_token` now checks the
internal token first. A request that fails `mint_allowed` is a guess and alone spends the strict per-peer budget (20 per minute,
unchanged; `tls_bundle_mesh` still counts every request against it). A legitimate mint spends a per-USER budget (the request's
`sub`, `HIVE_MINT_USER_PER_MIN`, default 30: a runaway-client bound, a session mints about every 15 minutes) and a peer-wide
backstop for all users together (`HIVE_MINT_BACKSTOP_PER_MIN`, default 600, the order of the admin router's own per-peer
budget). The UI's re-mint cadence is a separate change and is not touched. Witnessed on a real enforced node
(`scratch2/work/secfix6-mint.sh`, a fresh node per case): before, 3 defects (40 different users in a minute: 20 refused 429; 25
bad guesses and then a legitimate mint: 429; the backstop knob ignored, the peer capped at 20); after, 0 (40 users 40 minted,
one user 30 minted and 10 refused, the legitimate mint after 25 guesses 200 with the guesses still throttled, the backstop
capping exactly at its knob). Not rolled.

## 2026-10-14 — a browser-originated identity sync records nothing: the dashboard server is the only writer of the identity mirror (9f283c557f)

`26674b3fa7` kept a browser sync from writing an EMAIL, but `identity_sync` still recorded the rest of the body the caller
wrote: any signed-in caller (a viewer even, `/v1/identity/sync` is a viewer route) could POST a sync for ANY user id with the
name and image it liked, and the row took the CALLER's tenant (the reviewer's victim renamed `defaced`), could create a mirror
row for itself and rewrote the name and image of its own org. `identity_sync` now writes the mirror (`upsert_org`, the roster
mirror, `upsert_user`, `persist`) only for the dashboard SERVER's sync (`mint_allowed`: `x-hive-internal` on an enforced node,
always true on an unenforced dev node, which has no tenant boundary to protect). A browser-originated sync still answers 200
`{ok, tenant, is_owner}`, which is all the client reads; the server syncs the verified profile, tenant and org at every mint, so
nothing the browser sync recorded is lost. Witnessed on a real enforced node (`scratch2/work/secfix6-identity.sh`): before, 6
defects (a viewer's browser sync renamed the victim, replaced its image, moved its row to the viewer's tenant, renamed the
viewer's org `PWNED ORG`, replaced the org's image and created a mirror row for itself); after, 0, the server's sync still
recording name, image, email, org and roster and a dev node still recording (10 PASS); the reviewer's `t_transfer` chain agrees.
Not rolled.

## 2026-10-14 — a write nobody listed needs admin, and billing, DNS, the destructive database operations and the edge security rules are admin rows (3e737fa9c6)

The matrix defaulted a write no row names to MEMBER, so a member and the default CI key could charge the team (`POST
/v1/billing/charge`), edit DNS records and nameservers, delete or rotate a database, rewrite the WAF, rate-limit and bot rules
and the repository link, and a route added without a row opened itself to a member. `roles::floor` now answers ADMIN for a write
no row names, every route a member may call is NAMED in `RULES` (deploy and build, env, attaching and verifying domains, project
settings, snapshots, database create and connect, sandboxes, cron, queues, storage, workflows, microfrontend groups, drive,
routing, cache, secure links, incidents, the caller's own API key and team), and the new admin rows are billing checkout, confirm
and charge; DNS records, import, nameservers and certificate renewal; database delete and credential rotation; WAF, rate limits,
bot rules and SMS delivery providers; `PUT` and `DELETE /v1/gitops`. 37 routes change from member to admin (the operator-only
`/v1/admin/*` and the fleet-only mesh and internal routes among them, which no member could use before either). UI-visible:
`ui/lib/team-role.ts` (the client mirror of the table) must offer the new admin controls to an admin only. Witnessed on a real
enforced node (`scratch2/work/secfix6-matrix2.sh`): 19 named gap routes plus a sweep of every mutation route of the router x 7
principals (4 role JWTs, 3 key roles) = 1274 requests against an independent encoding of the intended floors; before 112
defects, after 0, with the day-to-day member flows and the default CI key's deploy, env, domain, queue, storage, database and
cron calls still passing and a dev node unchanged; the role-matrix (65) and CLI (8) witnesses still pass. Not rolled.

## 2026-10-14 — a transfer is a move: the project row is the owner of record, a claim never takes a project, and the deployments follow the row (e3effdb137)

`PUT /v1/projects/:p/team` (`413287e85e`) left the project's deployments tagged with the old team, and the old team kept the
project through them (`project_owned_by` was deployment-aware): it listed the deployments, read their resources and builds,
promoted and deleted them, aimed a queue consumer at the app, and its next env write TOOK THE PROJECT BACK, because the settings
write claimed the row with an unconditional `set_team` (after which it could delete it). `ProjectStore::claim_team` is now a
compare-and-set: an untagged row is claimed, the owner's own claim is a no-op, a row another team holds is refused
(`require_project_claiming_row` uses it). `admin::row_holder` is the owner of record, the team a row names when its tag is real,
and `admin::deployment_tenant` resolves a deployment's tenant through it: when a holder exists it ALONE decides, in
`project_owned_by` and in every gate that read a deployment's tag (the deployment list, resources, promote and delete, build read
and cancel, the queue-consumer target, the project list); the project delete and rename front doors refuse a foreign holder at
once (404, where a foreign delete used to cascade to the fleet and answer 502, after ~20 s on a fresh node). A transfer re-tags
the deployments of the node that serves it (`Gateway::retag_project`, with the function pools' tenant, `Fluid::set_tenant`), and
every other host converges from the row in `tenancy_reconcile` (`HIVE_DEPLOYMENT_RETAG_SECS`, default 15;
`HIVE_DEPLOYMENT_RETAG=0` disables), so a transfer made before this build completes itself on the first pass after the roll: the
old team stops listing those projects (the WARN `tenancy reconcile: re-tagged deployments to their project's team` names each).
Witnessed on real enforced nodes: a node that has not converged (`HIVE_DEPLOYMENT_RETAG=0` and the operator's row-only route,
`scratch2/work/secfix6-stale.sh`), before 17 defects (the old team listed the deployments and the project, read resources and a
real git build and its log, cancelled the build, promoted and deleted a deployment, aimed a queue consumer at the app, read the
settings, wrote env and took the project back, while the new team saw none of its own deployments and was locked out after the
take-back), after 0 with the new team served throughout; the transfer itself (`secfix6-move.sh`) before 9 defects, after 0,
including the convergence of a row-only change; the reviewer's `t_transfer`: the old team's env put 200 and a later delete 200,
now 403 and 404. Not changed, and open: a domain, database, webhook or integration the old team created is not moved by a
transfer; the browser lane (admission, artifact delivery, browser DB grants) still decides from deployment tags and follows them
within the re-tag interval; the mesh arms for delete, rename, reap and domain activation keep their own evidence (their engines
refuse a foreign row) and the delete arm reports acceptance for a delete its engine refused (a separate row). Not rolled.

## 2026-10-14 — a roster removal authorises every row it deletes, so a ghost row ahead of the Owner cannot carry the Owner away (8c1cf77c34)

`remove_checked` ruled on the FIRST row that matched an email and then deleted EVERY row that matched. With a legacy ghost row
ahead of the real Owner (an older build stored `" own@vt.test"`), an admin's `DELETE` of `"  own@vt.test"` (two spaces, which
matches neither row exactly) was judged on the ghost's role (member), answered 200 and removed both rows: the Owner was gone and
the last-owner rule never ran; the operator got the same through two Owner rows of one person. `check_roster_removal` is the
removal's twin of `check_roster_change` and takes the role of EVERY row the removal will delete: `AboveActor` when any of them
outranks the actor, `LastOwner` when the rows removed include every Owner the team has; `remove_checked` collects the targets,
authorises them together and only then drops them. The exact-string rule that keeps a ghost removable on its own is unchanged.
Witnessed with legacy rosters written into `state.json` (`scratch2/work/secfix6-ghost.sh`): before, 4 defects (the admin's padded
delete removed the Owner with 200; the operator's left the team ownerless); after, 0 (403 for the admin, 409 for the operator,
both Owners kept; removing a ghost by its exact string, a ghost and its member together and an owner removing an admin still
200; 5 PASS); the reviewer's `t_roster4` agrees. Not rolled.

## 2026-10-14 — a service role string opens nothing: the workflow queue token may only enqueue, fleet identities pass by subject and role, and every other service claim is no principal (31e80ccff2)

The role matrix (`d79bc7c1f0`) passed any claim whose role string was `service` through every row, and `git.rs` puts exactly such a
token in a tenant's project env: a workflow build issued `world-queue-client` (the team's tenant, 365 days) as `HIVE_QUEUE_TOKEN`,
which a member triggers with a one-line `vercel.json` in a zip upload. The real token, read from the node's data dir, answered 200
on SIEM, IP blocks, SCIM mint, webhook create, integration credentials, env put and delete on another project, project delete,
API-key revoke and deploy, opened protected previews (the preview gate trusts any verified tenant claim) and outlived the
member's removal; any forged `service` claim did the same. The fix is at the primitives. `auth::verify` accepts a service claim
only for a FLEET identity (`is_fleet_service`: `mesh-internal`, or the `mesh-node:<coordinator>` transfer token, subject AND
role) at the one decode every generic verification site shares, so the queue token and every other service claim stop being a
principal there. The queue client has its own verifier (`verify_queue_client`: subject AND role, never an operator, `iat` no older
than `HIVE_QUEUE_TOKEN_MAX_AGE_SECS`, default 30 days) and `auth::principal` is the one resolution `require_auth` and the regional
ingress share. `roles::gate` passes a fleet token, confines the queue client to `POST /v1/wqueue/enqueue` and `GET
/v1/wqueue/stats` and refuses any other service claim. A workflow build issues a 30-day token (not 365) and puts the PLAINTEXT
pair in the function env (it used to copy the sealed `enc:v1:` form of the project row, which is no bearer), and
`world_queue::rotate_queue_tokens` (every node at boot+90 s, then every `HIVE_QUEUE_TOKEN_ROTATE_SECS`, 6 h) re-issues the token
in the function env of each local deployment whose token is past half its age, expired, issued with a lifetime beyond the cap or
still sealed, through `Gateway::update_manifest` (future launches; a running instance keeps its token until it recycles); a
tenant's own `HIVE_QUEUE_TOKEN` is never touched. Migration for the tokens already in deployments' env: nothing to do for an app
whose instances recycle within 30 days of the roll (each local deployment gets a 30-day token about 90 s after its node boots
with this build); an instance that lives longer keeps its old token until it restarts, and the age cap counts from the token's
own `iat`, so a 365-day token issued N days before the roll stops working 30 days after ITS issue unless the instance was
recycled first or `HIVE_QUEUE_TOKEN_MAX_AGE_SECS` is raised; a project row's stale sealed copy is harmless (403 everywhere but
enqueue) and a rebuild replaces it. Witnessed on a real enforced node (`scratch2/work/secfix6-service.sh`): before, 34 defects
(the real member-triggered token 200 on eleven admin and member routes and an open protected preview; a 365-day token, an unknown
service subject and a forged `platform_admin` queue claim the same); after, 0, enqueue and the counters still 200, `mesh-internal`
and the `mesh-node` token still pass the gate, rotation replaces a 365-day token and a sealed copy with working 30-day tokens and
leaves a young token and a tenant's own alone (15 PASS); the reviewer's `t_queuetoken2` and `t_svc` agree. Not rolled.

## 2026-10-14 — the session cookie is re-minted from its own lifetime, once per browser, and an authorization 403 never re-mints (cf7298c6d5)

Org tokens live 900 s (`9e464c496d`) but the dashboard re-minted on a fixed 50-minute timer, so the `hive_jwt` cookie lapsed
under every poller and the only heal was the first anonymous reply (`x-hive-anon`), one mint per `AUTO_MINT_COOLDOWN_MS`:
pollers that saw the lapse in the same instant took the anonymous 200 and rendered it. The same primitive fed three more
mints into the backend's limiter (20/min per peer, and the dashboard server is the peer of every user): every 403 re-minted
(a member's webhooks page polls the operator-only `GET /v1/webhooks/deliveries` every 5 s: one mint per 30 s per tab), the
workflow console frame minted every 4 minutes and on every `visibilitychange`, and nothing kept two mints from running at
once in a tab. `/api/token` now echoes the cookie lifetime as `expires_in`, and each mint records `{issued, at, life, team}`
in a clock the tabs of a browser share (`ui/lib/session-clock.ts`: module state mirrored to localStorage
`hive_session_clock`, moved by `storage` events). One keeper per tab (`ui/lib/session-keeper.ts`, started by `SessionToken`)
re-mints at 70% of the lifetime +-5% jitter (the hour when a route predates the field), and a mint in any tab moves every
tab's mark, so one tab per browser mints per cycle; the keeper of a hidden tab never mints (a request such a tab makes
itself still waits for a mint), and a tab that becomes visible with under 20% of the lifetime left, or past its mark, mints
at once; a failed mint follows the existing budget (ladder 2/8/30 s,
`MINT_MAX_CONSECUTIVE_FAILURES`, then one attempt per period). `mintSessionToken` runs ONE mint at a time per tab with a 20 s
timeout (explicit mints, a team switch or an identity sync, still always fire, queued) and `renewSession` joins the one under
way. `ensureSessionMinted` makes a request wait only when no session exists or the credential is lapsing, so a woken tab
sends nothing anonymous and a proactive re-mint never holds requests. A rejection is retried without a mint when the cookie
was replaced after the request left, joins the mint under way, else re-mints once per cooldown as before; a 403 on a session
known valid for the tenant (role matrix, operator-only routes) never mints. Witnessed in headless Chrome on `next dev` plus
a throwaway enforced node with `HIVE_ORG_TOKEN_TTL_SECS=30` (the backend clamps it to 60 s), a member dev mint, 270 s of
polling `/settings/api-keys` (`scratch2/uifix/cdp-session-lapse.mjs`): before, one tab 5 mints and 4 anonymous replies (one
per lapse), three tabs 8 mints, 5 anonymous replies and two tabs minting in the same instant, a tab back after 100 s hidden 3
anonymous replies and the API-key list blank for 24.6 s; after, 0 anonymous replies and 0 401s throughout, mints every 39-45
s, three tabs = 3 load mints plus one per cycle, a hidden tab 0 mints while hidden and 1 within 3 s of visible and 0 s blank,
an outage keeps the budget (the suspend warning on the 4th failure, then one attempt per period, recovered 26 s after it
ended), and a refused role action costs 1 request and 0 mints (was 2 and 1). The real `api.ts`, `session-clock.ts` and
`session-keeper.ts` on a virtual clock against a fake browser and server: 55 checks (cadence, jitter bounds, one mint at a
time, hidden and visible, two tabs, failure ladder, gate, heal). Not changed: the webhooks page still polls the
operator-only deliveries route as a non-operator (a 403 every 5 s, now free of mints), and a workflow console in a hidden
tab is no longer kept alive by its own timer. Not rolled.

## 2026-10-14 — the dashboard offers only what the team role can do, and a role refusal reads as a sentence (8a7a6844b0)

The role matrix (`d79bc7c1f0`) refuses a viewer's or member's write with 403 `insufficient team role`, but every settings
page still drew the buttons, and the refusal surfaced as a raw body, an `alert()` or an unhandled rejection with nothing
shown. `ui/lib/team-role.ts` is now the ONE client mirror of `roles.rs`: `TEAM_ACTIONS` maps an action to its floor role and
the backend routes it covers (drift is a diff against `RULES`: a scratch script parses the 29 rows and checks every route's
floor, 15/15), `can(role, action)` answers it, and an unknown role (no mint yet, an unenforced node, a role the table does
not name) is offered every control, because the server is the authority. The role is the `role` the mint route returns,
mirrored to `hive_role` at every mint beside `hive_platform_admin` (`useTeamRole`, `useCan`; an operator counts as owner).
`RoleGate` (a disabled `<fieldset>` with a tooltip and a one-line note) wraps the webhook writes, integration links, the
owning-team and preview Save, the plan chips and the SSO switch, enterprise config (IP blocks, SAML, SCIM, SIEM, protection),
project delete (settings, project header, list menu), API-key create and revoke and team members; the API-key role list
stops at the caller's own role, and a webhook signing secret a member is not sent reads "visible to team admins". A 403 whose
body starts `insufficient team role` becomes an `ApiError` coded `ROLE_REFUSED` carrying one sentence ("Your role can't do
this. Ask a team admin.", or "...the team owner."), built once in `apiGet`/`apiSend` and shown inline or by `reportError` (a
toast), never an `alert`; the integrations link route and a plan change's `planRefusal` pass it through. `HIVE_DEV_ROLE`
(dev mint only) gives the local mint a role, which is how it was witnessed. Witnessed in headless Chrome on `next dev` plus a
throwaway enforced node (`scratch2/uifix/cdp-role-pages.mjs`, 8 pages, viewer/member/admin/owner/unknown): before, every
control was enabled for a member and a viewer and a click ended in a raw alert, an unhandled `ApiError` or raw text; after, a
member has the admin-floor controls disabled ("Needs a team admin") and a viewer also the member-floor ones ("Read-only:
needs team member access") with no request sent, admin and owner keep everything, and an unknown role keeps every control
and shows the sentence inline (0 alerts, 0 unhandled rejections). Not changed: `/api/token` still maps any Clerk org role
without "admin" to `member`, so a Clerk viewer is minted as a member today; `LocalTeams` still creates teams as `pro` (402
for a non-operator). Not rolled.

## 2026-10-14 — only the dashboard server writes the identity mirror's email, so a browser-originated sync can no longer pose as a team's admin or owner (26674b3fa7)

`POST /v1/identity/sync` wrote the body's user email into the identity mirror for ANY authenticated caller: the dashboard's
topnav calls it from the browser (its own cookie, no `x-hive-internal`) with a body the caller controls. The mirror feeds
authorization: `team_create` takes the new team's owner email from it, and the project-transfer rule (`413287e85e`,
`caller_administers`) takes the caller's roster identity from it. So an admin of one team could post the destination
team's admin email and then move a project into a team it does not administer (a bypass of that rule, found while
witnessing it), and any signed-in user could create a team whose owner row is a victim's email. The email is now set only
when the request is the dashboard SERVER's (`mint_allowed`: `x-hive-internal`, the Clerk-verified email its mint route posts
after every mint; an unenforced dev node stays open). A browser-originated sync still records the user and answers
`is_owner` and the tenant, and never sets or changes an email, so the topnav call is unaffected (it only reads `is_owner`).
Witnessed on a real enforced node (`scratch2/work/secfix5-identity.sh`): before, 3 defects (the mirror email rewritten, the
transfer into dest accepted, a team created with the victim as owner); after, 0 (the mirror keeps the verified email, the
transfer is 403, the team creation 412 for a user with no verified email) while the server-side sync still records the email
and the legitimate team creation under it works. Rows already written from a browser body before this change are not
repaired: the next server-side sync (every mint) overwrites the email with the verified one. Not rolled.

## 2026-10-14 — the roles a Clerk org assigns get a 15-minute token, so a demoted or removed member stops carrying the old role within minutes (9e464c496d)

A minted token carries its role for its whole life and nothing consults the roster per request (the dashboard takes the
role from Clerk when it mints), so an org admin demoted or removed in Clerk kept its admin JWT, and with it every
admin-floor action of the role matrix (`d79bc7c1f0`), for the full 3600 s. `mint_token` now asks `minted_token_ttl`:
`admin`, `member` and `viewer` live `HIVE_ORG_TOKEN_TTL_SECS` (default 900 s, clamped to 60..3600, an unparsable value
falls back to 900), while `owner` (the dashboard mints it only for a personal namespace), a platform operator (the
composio/system admin token, the ops console) and every other role, the `service` claim included, keep the hour. The
dashboard already honours `expires_in` wherever it mints (the `hive_jwt` cookie `maxAge`, the composio and ops server
tokens) and re-mints from the live Clerk session when the cookie lapses. The remaining window is the lifetime PLUS the
verifier's 60 s clock-skew leeway (`jsonwebtoken`'s default): about 16 minutes instead of 61. Witnessed on a real
enforced node (`scratch2/work/secfix5-ttl.sh`): before, an org admin, member and viewer token lived 3600 s; after, 900 s
(`expires_in` 900), owner, the operator (also with the role claim admin) and a service claim 3600 s, the knob clamps 10 to
60 and 99999 to 3600, and with a 60 s lifetime the admin token creates a webhook while valid, is refused 401 once the
lifetime and the leeway have passed, and the same user re-minted as a member is refused the admin action (403) while
day-to-day work still passes. Operator note: set `HIVE_ORG_TOKEN_TTL_SECS` shorter (60 is the floor) to tighten the
window or `3600` to restore the old one; to cut a session NOW rotate `HIVE_JWT_SECRET` (every token ends); a Clerk
sign-in was read, not run (no Clerk instance). Not rolled.

## 2026-10-14 — a roster email is trimmed and matched case-insensitively at the store, an unusable one is no member, and removing nobody purges and revokes nothing (e62c368781)

`TeamStore` matched emails by lowercase only, so `" own@vt.test"` (a leading space) added a GHOST row beside
`own@vt.test` instead of reaching its owner protection, `"  new@vt.test "` was stored with its spaces and the clean address
then made a duplicate, an empty or blank email was accepted and took a seat, and removing an email nobody had still ran the
push purge and revoked every browser grant of the team. `roster_email` now trims and refuses an empty, over-320-byte or
control-character email (`RosterRefusal::BadEmail`, 400, checked before the seat limit) and `same_person` is the one
comparison `Team::member`, `set_member_role` and the removal use, so a padded or upper-cased address is the same person and
reaches the owner and last-owner rules (an admin's padded removal of the owner is 403). `add_member_if_absent` (SCIM, SAML
auto-provision, the Clerk mirror) ignores an unusable email; `remove_member_as` returns `Ok(None)` for a non-member and
`team_remove_member` answers 404 before any purge or revoke. A ghost row an older build stored stays removable on its own (a
removal matches the row stored under exactly that string first, a blank one targets blank-email rows), and rows are not
rewritten at boot, because a load-time rewrite would not converge with older peers still replicating them. Witnessed on a
real enforced node (`scratch2/work/secfix5-roster.sh`): before, 10 defects (the ghost row, a padded address stored raw, a
padded plus clean duplicate, a case-variant duplicate, three unusable emails taking three seats, removal of nobody 200);
after, 0, including the legacy leg on the same data dir (an older build writes `" mem@vt.test"` and an empty-email row, the
new build removes each on its own and leaves `mem@vt.test` a member); the reviewer's `t_roster2` and `t_roster3` agree. Not
rolled.

## 2026-10-14 — a project moves to another team only into a tenant the caller administers, or by the operator into a team that exists (413287e85e)

`PUT /v1/projects/:p/team` wrote the body's team string straight into the project row: with the role floor of the matrix an
admin of one team could still hand a project, its settings and its env to a tenant of their choosing (`"attacker"`), park it
in a tenant that does not exist, or store a sentinel (the empty team, which `norm` reads as the OPERATOR's `personal`,
`__anon__`, `__untagged__`, `mesh`) or a path-like name. The handler now decides where the project may go. The owning team can
always save itself again (the dashboard's preview-protection toggle). A move to ANOTHER tenant needs the platform operator,
into a team that exists (404 otherwise; the unvalidated repair route stays `POST /v1/admin/projects/:p/team`), or a
destination the caller administers: its own personal namespace (`u_<uid>`), or a team whose roster lists the caller's
verified identity (the identity mirror maps the JWT subject to its email) at admin or above, 403 otherwise; an API key is no
user and administers nothing. The destination must be a tenant slug for everyone (400), a transfer is on the audit log, and an
unenforced dev node stays open apart from the slug shape. Witnessed on a real enforced node
(`scratch2/work/secfix5-transfer.sh`): before, 12 defects (an admin moved a project to the attacker tenant, which then read
its settings and wrote its env, an owner moved one into a team it is not on, empty, `__anon__`, `__untagged__`, `mesh` and
path-like destinations accepted, the operator parked projects in tenants that do not exist); after, 0, with the same-team
toggle, a move into the admin's own namespace, into a team where its verified email is an admin and the operator's move into
an existing team all still 200. A transfer does not re-tag the project's deployments, so the previous team still reaches the
project through them (`project_owned_by` is deployment-aware): unchanged. Not rolled.

## 2026-10-14 — the team-role matrix is defined once and enforced at the one gate every request passes, so a viewer is read-only and webhooks, integrations, project ownership and project delete need an admin (d79bc7c1f0)

The role model had no single rule: each handler re-decided its own floor and some never did, so `e2ff4bc20d` closed eight
enterprise routes and left their siblings open. A viewer (a viewer API key, or a viewer JWT) deployed, deleted projects and
every deployment in them, wrote webhooks, integrations, microfrontend groups and the conformance policy, read an
integration's plaintext credentials, and moved a project to another tenant with preview protection off; a member did the
admin-only writes too. `crates/hive-cloud/src/roles.rs` now holds the matrix and `auth::require_auth` asks it for every
authenticated request on an enforced node, reads included. A role is the verified `role` claim of the JWT or API key,
`viewer < member < admin < owner`; the first matching row of `RULES` wins, and a request no row matches needs viewer to read
and member to change anything:

- **viewer** reads. Its only mutations are the self-service rows: its own notifications, push and SMS delivery, the browser
  node lane, the profile sync (`/v1/identity/sync`) and a vector query. (`POST /v1/token` is an open route.)
- **member** does the day-to-day project work, the floor of every mutation no row raises: deploys, env, domains, databases,
  storage, queues, sandboxes, cron, workflows, microfrontend groups, the conformance policy, creating a key no higher than its
  own role. Reads that hand out a way to write or run code are member too: database credentials, a sandbox shell, a realtime room.
- **admin** runs the team: webhooks (and the signing secrets in their list), integrations and their credentials, API-key
  revocation, `PUT /v1/projects/:p/team` (protection and ownership), `DELETE /v1/projects/:p`, the rest of the enterprise
  configuration (IP blocks, SIEM, SAML, SCIM, deployment protection), team settings and membership below its own rank.
- **owner** does everything an admin may, plus deletes the team.

One deliberate deviation from "owner for project delete and transfer": the dashboard mints `owner` only for a personal
namespace and a Clerk org's highest JWT role is `admin`, so an owner floor would lock every org out of both; they sit at admin.
A platform operator acts as an owner; a `service` claim (a fleet delegation, the workflow queue client) is not a team role and
is judged by its own handler; an unknown role is the lowest, never an implicit member; an unenforced dev node has no verified
roles and stays fully open (`require_auth` returns before it asks). The duplicates are gone (`require_config_admin`, the env
and API-key-revoke floors, `roster_actor`, which is `roles::actor` now); `admin::require_team` still binds a path slug to the
tenant. Operator and CI notes: a dashboard member now sees a 403 on webhooks, integrations, the owning-team Save and project
delete (the buttons are still shown); `shadw` webhook and project-delete commands need an admin key (a default-role key still
deploys, sets env, attaches domains and creates keys); a member's webhook list shows the hooks without their signing secrets.
Witnessed on a real enforced node with JWT and API-key roles (`scratch2/work/secfix5-matrix.sh`): before, 47 defects; after,
0 with every day-to-day and self-service flow, the operator, the service claim and the dev node still passing (65 PASS, 0
FAIL); the reviewer's `t_projteam`, `t_viewerops` and `t_entcfg` agree, and so does the real `shadw` CLI against API keys of
each role (`secfix5-cli.sh`: before, a viewer key deleted a project and a default key created a webhook; after, both are
refused with "this needs admin or above" while env, listings and an admin key's delete and webhook still work). Not rolled.

## 2026-10-14 — a model download is bounded by throughput and by tenant, so one tenant's slow-drip URLs cannot hold the node's download slots for hours (914a82e577)

A slow drip (a byte every few seconds) delivers data often enough that the stall timeout of `a21318ab2c` never fires,
so a tenant whose projects named two such URLs held both of the node's two download slots for the whole 2 h total
timeout and no other tenant's healthy model could start. `ensure_model` now ends a transfer whose bytes over a whole
rate window (`HIVE_INFERENCE_MODEL_RATE_WINDOW_SECS`, 30 s) fall under `HIVE_INFERENCE_MODEL_MIN_BYTES_PER_SEC`
(64 KiB/s; 0 disables) with "model download too slow", and the stall default drops from 60 s to 30 s, so a trickle
holds a slot for one window, not for the total timeout. `TenantSlot` caps a tenant at
`HIVE_INFERENCE_MODEL_DOWNLOADS_PER_TENANT` (1) running download, taken before the node-wide slot and released with
it on every exit of the download task; the tenant is `record_tenant(team_of(project))`, so every untagged project
shares the fail-closed sentinel's one place. Witnessed on a real unenforced dev node that coordinates
(`HIVE_GPUS`), two tenants and a stand-in model host (`scratch2/work/secfix-model3.sh`, `secfix-model4.sh`): before,
alpha's two drip projects held both slots and beta's healthy 4 KiB model started only when the 60 s total timeout
cut them, 58 s after it was deployed; after, alpha holds one slot (the second drip was requested only after the
first was cut), beta's model downloaded 3 s after it was deployed and each drip was cut with "model download too
slow" after one 6 s window. A healthy but slow 3 MiB model at 256 KiB/s still completed and a steady 16 KiB/s
transfer was cut. Operator note: a model host that serves under 64 KiB/s needs
`HIVE_INFERENCE_MODEL_MIN_BYTES_PER_SEC` lowered (0 turns the floor off). Left open on purpose: as many colluding
tenants as there are node-wide slots (`HIVE_INFERENCE_MODEL_DOWNLOADS_MAX`, 2) can still occupy all of them for one
window at a time. Not rolled.

## 2026-10-14 — a DB-gateway session is proven only once its database accepts authentication, and a full pending budget evicts its oldest stuck connection instead of starving everyone else (d051d5fd50)

The gateway of `a67a44b67a` counted a session as proven on ANY backend reply. A Redis with a password answers `-NOAUTH`
to everything but `AUTH`/`HELLO`/`QUIT`/`RESET` (and `+RESET` to a client that never authenticated) and a Postgres
backend answers a StartupMessage with an authentication request, so one byte answered `-NOAUTH` released the session
from every bound: it stayed open for as long as the client liked (still open at 12 s with the knobs at 3 s), costing
the gateway two sockets and the database a connection slot. `AuthProof` now proves a session only when authentication
was ACCEPTED (a Postgres `AuthenticationOk`; a Redis `+OK` to `AUTH` or the map/array `HELLO` answers when the
database has a password, any non-error reply when it has none; errors and `+RESET` prove nothing). Until then the
session has `HIVE_DB_GATEWAY_AUTH_DEADLINE_MS` (30 s, absolute from the first forwarded byte; it replaces
`HIVE_DB_GATEWAY_FIRST_REPLY_MS`, which was never rolled), and an authenticated idle session keeps no bound at all.
The pending budget, taken at accept before TLS, was 2048 global and 256 per address: a few addresses' half-open
connections filled it and every other client was reset, and 800 legitimate clients restarting behind ONE NAT address
had 544 turned away. It is now a `PendingTable` (global 4096, `HIVE_DB_GATEWAY_PENDING_MAX`; per address 1024,
`HIVE_DB_GATEWAY_PENDING_PER_IP_MAX`, an IPv6 source as its /64): a full budget evicts the oldest connection that has
sat unproven for `HIVE_DB_GATEWAY_PENDING_STUCK_MS` (2 s; the source's own oldest when the source is at its cap, the
globally oldest when the table is full) and refuses a newcomer only when nothing in it is stuck. The failure WARN goes
through a `Throttle` (one line per 10 s with the suppressed count; `debug` shows every one). Witnessed on a real node
running the gateway with a seeded self-signed `*.db.witness.test` bundle and a stand-in engine speaking Redis and
Postgres (`scratch2/work/secfix-dbgw4.sh`): before, the `-NOAUTH`, `+RESET` and Postgres-request sessions were all
still open at 12 s, two addresses holding 10 half-open connections each starved a third address and an address at its
cap, an 800-client burst from one address had 544 turned away and 300 failed connections wrote 300 WARNs; after, those
three sessions are cut at 3.0 s (knob 3 s) while an authenticated Redis session, a pipelined PING+AUTH session and a
full Postgres password exchange stay open (the authenticated one for 150 s at the default knobs), both clients get in
during the flood, 800 of 800 authenticate and 300 failures write 1 WARN. The bound that remains: a burst of more than
1024 simultaneous connections from ONE address has its excess refused at once and reconnects; raise the per-address
knob for a larger NAT (`LimitNOFILE` is 1048576 on a platform node, so 4096 pending connections cost no descriptors
worth counting). Not rolled.

## 2026-10-14 — the dashboard's nameserver card shows only measured or user-entered nameservers (01aaf526de)

The DNS-only domain card's "Restore Original" button PUT the literal `ns1.openedge-dns.com` /
`ns2.openedge-dns.com` (a pair no node serves; `71d29ab090` stopped the node seeding it and empties it on load),
under a description that said OpenEdge propagates its nameservers by default, so a tenant who clicked it recorded
two nameservers that do not exist. A DNS-only domain now shows the platform roster the nodes measured
(`fetchNsRoster`, listed only once `delegation_ready`, otherwise "still converging") in the same Nameservers
section an attached domain uses, plus a "Custom nameservers" section holding only what the tenant typed; the reset
button is "Clear" (writes `[]`, shown only when there is something to clear) and the literal is gone from `ui/`.
Witnessed with headless Chrome over CDP against `next dev` and a throwaway dev node
(`scratch2/work/secfix4-ui-ns.sh`): before, clicking "Restore Original" on a domain with its own nameservers wrote
`["ns1.openedge-dns.com","ns2.openedge-dns.com"]` and one ui source file carried the literal; after, the rendered
page has no occurrence, "Clear" writes `[]` and no ui source names it; `npx tsc --noEmit` and `npm run lint` are
clean. Operator note: the dashboard rolls separately (`scripts/deploy-ui-fleet.sh`); until it does, the old button
keeps writing the pair, which a node only empties when it next loads its domain store. Not rolled.

## 2026-10-14 — nobody changes or removes a team member above their own role, and the last owner can never be removed or demoted (557f3687a8)

`POST /v1/teams/:slug/members` overwrote an existing member's role and `DELETE /v1/teams/:slug/members/:email`
removed any row, both gated only by owner/admin: an ADMIN demoted or removed the team's owner (the only check was
that an admin cannot GRANT Owner), an owner or a platform operator removed or demoted the LAST owner and left the
team ownerless, and an admin demoted one of two owners. The rule now lives in the team store as the one primitive
every roster change obeys (`check_roster_change`): nobody changes or removes a member of a higher role than their
own, nobody grants a role above their own, equal rank is allowed (an admin may change an admin), and the last
Owner is never removed or demoted, the owner themselves and a platform operator included.
`TeamStore::set_member_role` and `remove_member_as` replace the unchecked `add_member`/`remove_member` (deleted, so
no path can bypass the rule); `remove_plain_member` (SCIM deprovisioning) is the same rule with a Member as the
actor, so an owner or admin stays out of a directory sync's reach by the same code. The HTTP layer maps the actor
from the verified JWT role (a platform operator acts as Owner, an unenforced node is open), answers 403 for an
above-actor change or grant and 409 for the last owner, and returns before anything is purged or revoked.
Witnessed on a real enforced node (`scratch2/work/secfix4-roster.sh`): before, 7 defects (an admin demoted the
owner, an admin removed the owner, the sole owner demoted and removed themselves, a platform operator removed the
last owner, the remaining owner of two stepped down, an admin demoted one of two owners); after, 0: 403 and 409
with the roster unchanged, while admin-vs-admin (demote, remove), an admin promoting a member to admin and removing
a viewer, owner-vs-admin (demote, remove), granting Owner, a second owner, stepping down with two owners and
removing one of two owners all still answer 200. Operator note: a team that was already left with no owner by the
old routes is not repaired here (the rule only protects an owner who is still there); list teams with
`GET /v1/teams` and grant one deliberately. Not rolled.

## 2026-10-14 — a team's enterprise configuration is written by an owner or admin, never by a viewer or member (e2ff4bc20d)

IP blocks (add, delete), the SIEM sink and its test event, the SAML config and deployment protection were
`tenant()` plus a plan gate with no role gate, so any team VIEWER or MEMBER reconfigured the team's security:
pointed the audit stream at their own SIEM endpoint, added or deleted IP blocks, switched deployment protection
off, rewrote the SAML IdP; the SCIM gate of `73fa118212` was the same hole closed for one route. One helper,
`require_config_admin` (`require_team_floor` with owner and admin), now fronts every one of those writes and
SCIM's (it replaces the SCIM-only `require_scim_admin`); reads stay open to every member and an unenforced dev
node, with no verified roles, stays open like every other role gate. The dashboard's enterprise settings page and
the project protection card write through the user's own JWT (an admin or owner keeps working; a viewer or member
now sees the 403 body); the GitOps export only reads and no CLI calls these routes. Witnessed on a real enforced
node (`scratch2/work/secfix4-entcfg.sh`, team on enterprise, JWT roles owner/admin/member/viewer): before, a viewer
and a member could do all six writes (12 defects, 200 each); after, viewer and member get 403 on all six, admin
and owner get 200 on all six, every member (viewer included) still reads all five GET routes, and an unenforced
dev node answers 200 on all six. Left open on purpose: microfrontend groups and the conformance policy
(project-level config a member may legitimately edit). Not rolled.

## 2026-10-14 — a Clerk org's team is created at Hobby, so its record and its billing account agree and no org reaches Pro for free (d57615ef87)

`identity_sync` mirrors a Clerk org into the teams store with `create_with_slug(slug, name, "pro", ..)`,
outside the plan-authority primitive: the team record read Pro while the org's billing account (Hobby until a
checkout or an operator grant) read Hobby, the two halves `apply_plan_everywhere` exists to keep in step, and
every org reached Pro-gated features (deployment protection, failover, longer runtimes) with no payment. It is
now created at Hobby, the plan `team_create` gives a non-operator: a tier is raised only through
`apply_plan_everywhere` (a paid checkout or an operator), and the billing account of a new tenant is Hobby, so
both halves agree without a second write. Witnessed on a real enforced node (`scratch2/work/secfix3-small.sh`,
the dashboard's server-side identity sync with `x-hive-internal`): before, the new org's team record read pro,
its billing account hobby, and the org owner's PUT of deployment protection (a Pro/Enterprise feature) answered
200 with no checkout; after, both read hobby and the same PUT answers 403, while an operator grant (both halves)
still opens the feature (200) and an operator still creates a Pro team directly (200). Operator note: org teams
created by `identity_sync` before this change keep the plan they were stored with (pro) while their billing
account reads hobby; list them with `GET /v1/teams` and lower or grant them deliberately. Not rolled.

## 2026-10-14 — the deployment-password unlock cookie is a server-sealed expiring token, Secure outside plain-http loopback (229e0df6d9)

`EnterpriseStore::seal_password_cookie` returned the STORED password hash as the `hive_pw` cookie value and
`verify_password_cookie` compared the cookie with it, so the cookie was password-equivalent (replayable, with no
server-side expiry, for as long as the password stayed the same), handed the Argon2 hash itself to anyone who
could read the cookie, and was set without `Secure`. The cookie is now `v1.<expiry ms>.<hex HMAC-SHA256>`: the MAC
covers the deployment, the expiry and the stored hash and is keyed from `secrets::key_material()` (the
fleet-shared at-rest key, so any node verifies a token any node issued), compared in constant time. A token stops
at its own expiry (`HIVE_PW_COOKIE_TTL_SECS`, default 24 h), is bound to its deployment and dies when the password
changes; the stored hash opens nothing. `Set-Cookie` keeps `HttpOnly` and `SameSite=Lax`, `Max-Age` equals the token
lifetime and `Secure` is set unless the request is plain http to a loopback dev host (`localhost`, `*.localhost`,
`127.0.0.1`, `::1`) without `x-forwarded-proto: https`, so the unlock still works on a plain-http localhost dev
node. `shadw_zk` is untouched. Witnessed on a real enforced node over the public listener
(`scratch2/work/secfix-pwcookie.sh`): before, the value was the `$argon2id$` hash, no `Secure` on a public host or
behind `x-forwarded-proto: https`, and the cookie still opened the site after the configured lifetime; after, the
value is `v1.<expiry>.<mac>`, `Secure` on a public host and behind the proxy header, absent on plain-http
`pwa.localhost` where the unlock still works, a later expiry with the old MAC, a flipped MAC and a made-up token
401, one deployment's token on another 401, the same token after a 4 s lifetime 401, a changed password
invalidates earlier cookies. Operator note: cookies issued before the roll stop opening the gate (each visitor
enters the password once more), and verification needs the fleet-shared `HIVE_SECRET_KEY` like the TLS tickets
do. Not rolled.

## 2026-10-14 — model downloads share one node-wide concurrency limit and one disk reserve (a21318ab2c)

Each download was admitted and re-checked against the free space on its own: with a 2 GiB reserve and 32 GB
free, three downloads that each declare 18 GB all passed (18 + 2 < 32) and wrote at once, so N concurrent
downloads overshot the reserve N-fold, and nothing bounded how many ran. A `DiskClaim` ledger now holds the bytes
every download in flight still expects to write; a download is admitted only when the volume keeps the reserve
after the others' remainders and this file, the 256 MiB re-check asks the same of everything in flight, and the
claim drops on every exit. A node-wide semaphore (`HIVE_INFERENCE_MODEL_DOWNLOADS_MAX`, default 2) limits running
downloads; a URL with no free slot is tried again on the next tick, nothing queues. Witnessed on a real node
(`scratch2/work/secfix-model2.sh`): four slow-drip downloads ran four at once before (4 partial files) and two at
a time after; three 18 GB downloads against 32 GB free were all admitted before (3 partial files) and exactly one
after, the others failing "insufficient disk space for the model". Not rolled.

## 2026-10-14 — a DB gateway session is bounded while it is unproven and while it drains, capped per source, and a NAT burst is no longer reset (a67a44b67a)

After the first client byte nothing bounded a session: `copy_bidirectional` ends only when BOTH sides reach EOF, so a
client that stayed open after the engine closed the session (Redis `QUIT`) pinned the gateway's two sockets for good;
one byte then silence held a database connection slot indefinitely (the previous wave cut it at 10 minutes, this one
at never); one source address could hold any number of sessions; and the default of 32 pending connections per
address reset a legitimate NAT'd fleet reconnecting at once. `splice` now copies the two directions itself over a
shared phase: the backend must send its first reply within `HIVE_DB_GATEWAY_FIRST_REPLY_MS` (30 s; a legitimate
client is answered at once, one byte then silence never is), the pending slot is held until that reply, and once
either direction ends the survivor may sit silent for `HIVE_DB_GATEWAY_LINGER_SECS` (60 s) of inactivity (each read
restarts the clock, so a large reply still draining to a half-closed client is never cut). A proven session has no
idle bound at all (pools and `LISTEN` stay up). The sessions one SOURCE address may hold at a backend are capped
(`HIVE_DB_GATEWAY_SESSIONS_PER_IP_MAX`, 4096) before the backend is touched; per source, not per database (one
address could lock a tenant out of its own database) and not global (one address could lock out every tenant).
Pending defaults: per address 32 to 256, global 512 to 2048. Witnessed on a real node with a seeded TLS bundle and a
line-protocol stand-in engine (`scratch2/work/secfix-dbgw3.sh`): after the engine's `QUIT` with the client open, the
gateway-side socket sat in `FIN_WAIT_2` at 10.5 s before and was gone after; one byte then silence stayed open past
10 s before and was cut at 3.0 s (first-reply knob 3 s) after; a pooled idle session survived before and after, and
with the default knobs lived 150 s idle and answered a second command; 8 sessions from one address were 8
established before and 4 after (cap 4, the other 4 turned away before the engine saw them) while a second address
still got its sessions; 100 clients from one address completing TLS at once were 32 admitted and 68 turned away
before and all 100 admitted after. Not rolled.

## 2026-10-14 — the world's loopback exemption accepts the REST URL of any of the project's own Redis databases (ef100e0607)

`DatabaseStore::local_redis_rest_url` returned the FIRST matching record's REST URL and `world_config` accepted a
loopback URL only when it equalled that one, but every managed Redis database's egress writes its own
`UPSTASH_REDIS_REST_URL` into the project env and the last one attached wins the key: with two Redis databases on a
node with no `HIVE_DB_DOMAIN` the env named the second while the check read the first, so the platform's own sidecar
was refused and the world read nothing. `local_redis_rest_urls` returns the URLs of ALL the project's Redis records
hosted on this node (same tenant, same project) and the exemption accepts a loopback URL equal to any of them; a
loopback URL no record holds is still no world at all. Witnessed on a real node with two Redis databases for one
project (`scratch2/work/secfix-world2.sh`): with the env as the databases' egress left it, 0 world reads reached
either database before and the read reached the one the env names after; restating the second database's URL was
refused before and reached after; a tenant-set arbitrary loopback port got 0 requests before and after. Not rolled.

## 2026-10-14 — a team-private preview opens only for a credential of the owning team or a platform operator, never for any tenant's owner role (ffa3e09d15)

`edge::has_preview_access` accepted a verified JWT, as a Bearer or as the `hive_access` cookie, when its tenant matched
the project's team OR its role was `owner`. `owner` only means owner of the caller's own tenant (every user owns their
personal namespace), so any other tenant's owner JWT, and any personal-namespace owner, opened every team-private
preview; a viewer JWT got 401. `claims_open_preview` is now the one question both credential paths ask:
`platform_admin`, or the caller's tenant equals the project's stored team through `admin::record_tenant` (a lost tag
resolves to the fail-closed `UNTAGGED_TENANT`, which no claim equals). A repository-wide search for the same shape (a
`role == "owner"` accepted without a tenant match) found no other site. Witnessed on a real enforced node over the
public listener with two tenants (`scratch2/work/secfix-preview.sh`): before, tb's owner JWT opened ta's private preview
as a Bearer and as the cookie, and a personal-namespace owner JWT did too (200 each); after all three are 401, while
ta's owner and viewer, the operator and every control (no credential, tb viewer, tb admin, tb's API key, a garbage
cookie) are unchanged. Not rolled.

## 2026-10-14 — a DNS-only domain record claims no nameservers the platform never measured (71d29ab090)

`dns::default_domain` seeded every record with nameservers `ns1.openedge-dns.com` and
`ns2.openedge-dns.com`: no such zone is delegated or measured, and the dashboard's Nameservers card showed
them as the domain's. A new record now carries none, and `DomainStore::load` empties a record whose
nameservers are ONLY that invented pair (written by an earlier build or replicated from a peer on one); a
tenant's own nameservers, or any list with a non-invented name in it, are kept as set. The key stays (an
older node parses `state.json` and snapshots with `nameservers` required): an empty list is what a record
that has set none carries. Witnessed on a real node (`scratch2/work/secfix3-small.sh`): before, a fresh
record served the invented pair and a persisted record kept serving it after a restart; after, a fresh
record serves `[]` and the persisted pair is scrubbed on load, while `custom.example`'s own nameservers and
a mixed list survive the load. The dashboard's "Restore Original" button still writes the invented pair from
a literal in `ui/` (outside this change). Not rolled.

## 2026-10-14 — the SAML ACS role gate steps aside on an unenforced dev node like every other role gate (42e5d703fa)

`c45cbe1508` gated the SAML ACS with plain `admin::require_team(..., [owner, admin])`. With no
`HIVE_JWT_SECRET` there are no verified claims and so no roles: every dev-node caller got 403, where every
other role gate steps aside (`operator_allowed`, `require_team_floor`, the env and key floors). It now uses
`require_team_floor`: unchanged with auth enforced (anonymous 401, another team's caller 403, a plain member
403, the team's own owner or admin passes), open on an unenforced node. Witnessed on real nodes
(`secfix3-small.sh`): before, the dev-node ACS POST answered 403 and provisioned nothing; after, 307 to the
dashboard with the NameID on the roster; the enforced matrix is identical before and after and the earlier
`secfix2-saml-scim.sh` still passes. Not rolled.

## 2026-10-14 — a Stripe checkout is paid when Stripe says its payment_status is paid, in the read-back and in the signed webhook (8d1a8377bf)

After `fb3b773810` the batch-3 review found, live: `stripe_verify_session` (the `POST /v1/billing/confirm`
read-back) treated `status == "complete"` as paid whatever `payment_status` said, and the
`checkout.session.completed` webhook arm never read `payment_status`: a signed UNPAID completion credited the
account and set the enterprise plan. A delayed payment method (a bank debit) completes the session while
`payment_status` is still `unpaid`: the money has not arrived and can still fail. Both now ask
`billing::session_paid` (`payment_status == "paid"`). `no_payment_required` is refused too: this platform
creates no zero-amount session (a free plan or a $0 addon is applied directly and never reaches Stripe), so
it could only mean a trial or coupon nobody configured. A delayed payment still completes: confirm succeeds
once Stripe reports it paid and the webhook now also handles `checkout.session.async_payment_succeeded`
(`async_payment_failed` is logged, nothing applied); a paid event applies through `confirm_checkout`, which
consumes the record, so a replayed delivery is a no-op. To witness the Stripe paths on a real node the Stripe
API base is now `STRIPE_API_BASE` (default `https://api.stripe.com`; only `https://`, or `http://` to a
loopback host, is honoured because the secret key rides every request as basic auth; anything else is logged
and ignored). Witnessed on a real enforced node pointed at a loopback fake of the two Stripe endpoints, with
signed payloads through `POST /v1/billing/webhook` (`scratch2/work/secfix3-payment.sh`, guard in
`secfix3-knob.sh`): before, 4 defects (an unpaid-but-complete session confirmed and credited,
`no_payment_required` credited, an UNPAID completed event credited the account, an unpaid completed event set
the enterprise plan); after, none: 402 or ignored with balance and plan unchanged, while a paid session, a
paid completed event and a delayed payment reported paid (confirm and `async_payment_succeeded`) all apply, a
replay is a no-op, and a wrong secret or a stale timestamp is 401. The success path against real Stripe is
unchanged and unwitnessed here. Operator note: a delayed-method purchase now waits for the webhook (set
`STRIPE_WEBHOOK_SECRET`) or a later confirm. Not rolled.

## 2026-10-14 — SCIM is managed by a team owner or admin, and a directory sync never changes or removes the people who run the team (73fa118212)

After `c45cbe1508` opened the SCIM routes the batch-3 review found, live: `POST` and `DELETE
/v1/enterprise/scim` had no role gate (a viewer or member minted the IdP bearer, and re-minting 401s the IdP's
current token, or disabled directory sync); `scim_users_create` added members with `TeamStore::add_member`,
which overwrites an existing member's role, so provisioning the owner's email demoted the owner to member, the
admin's too, and a viewer's promoted the viewer; SCIM PATCH `active:false` and DELETE removed whatever roster
member carried the SCIM user's `userName`, an admin or the owner included. Enable and disable now need owner
or admin (`admin::require_team_floor`, which steps aside on an unenforced node). The directory adds through the
new primitive `TeamStore::add_member_if_absent` (atomic: an existing member keeps its role) and removes through
`TeamStore::remove_plain_member` (an Owner or Admin is left in place and the deprovisioning answers 403 in
SCIM's error envelope with nothing changed, so the IdP sees the refusal). The same add-if-absent primitive now
backs the SAML ACS auto-provision and `identity_sync`'s Clerk org mirror (which had an inline is-member
check). Witnessed on a real enforced node (`scratch2/work/secfix3-scim.sh`): before, 9 defects (viewer and
member minted the bearer and disabled SCIM, the IdP locked out, owner and admin demoted and a viewer promoted
by provisioning, PATCH removed the admin, DELETE removed the owner); after, none: 403 for viewer and member,
the IdP bearer untouched, roles unchanged, 403 on the owner/admin deprovisioning with the roster intact, while
a new email is still added as a member and a plain member is still deprovisioned by PATCH and DELETE (the
earlier `secfix2-saml-scim.sh` still passes). Not rolled.

## 2026-10-14 — the preview unlock cookie is Secure on every host but a loopback dev one (127269cc21)

`zkauth::issue_cookie` set `Path=/; HttpOnly; SameSite=Lax; Max-Age` with no `Secure`. It now carries
`HttpOnly; Secure; SameSite=Lax` except where the host is `localhost`, `*.localhost` or a loopback address
(a browser on plain `http://<project>.localhost:<port>`, the local dev flow, would not keep a Secure cookie
in every browser); the scheme of the unlock request is not visible to `bootstrap` (`edge.rs` passes the host),
so the host decides. `issue_cookie` also refuses to issue a cookie it could not seal (`secrets::encrypt`
hands its input back unsealed on a failure and `cookie_access` accepts only sealed values): `bootstrap`
answers 500. Witnessed (`scratch2/work/secfix3-zkcookie.sh`): before, the cookie of host
`victimproj.witness.test` read `Path=/; HttpOnly; SameSite=Lax; Max-Age=3600`; after, `Path=/; HttpOnly;
Secure; SameSite=Lax; Max-Age=3600`, while `victimproj.localhost` still issues it without Secure. Not
rolled.

## 2026-10-14 — a cookie or stored password is checked against a value some key actually opened, never against whatever string was sent (51dd177a1c)

`secrets::decrypt` is a passthrough: it returns its input unchanged when the input is not an `enc:v1:` value
or no key opens it. `zkauth::cookie_access` read the visitor's `shadw_zk` cookie through it, so a plaintext
`Cookie: shadw_zk=<project>|99999999999999` (never issued by the node) parsed as a valid unexpired unlock and
opened every team-private preview and Standard-protected deployment (zkauth is default-on in fleet builds).
`enterprise::password_matches` had the same shape in its legacy `enc:v1:` branch: an envelope no key opens (a
key change without `HIVE_SECRET_KEY_OLD`) compared equal to itself, so the ciphertext submitted as the
password unlocked the deployment. Both use `secrets::try_decrypt` and fail closed; a value that does open
still works. The other `decrypt()` callers were swept and left (env vars, sandbox env, TLS key PEMs, webhook
signing keys and SIEM tokens are handed to the owner's own workload, none decides access) and `decrypt`'s doc
now forbids letting its result decide access. Witnessed on a real enforced node
(`scratch2/work/secfix3-zkcookie.sh`): before, forged cookies `victimproj|99999999999999` and
`victimproj|<now+1h>` answered 200 on the private preview and the ciphertext submitted as the password
unlocked (307 password-unlocked); after, 401 each, while the node's own sealed cookie unlocks (also after a
restart) and the legacy sealed password still verifies. Not rolled.

## 2026-10-13 — the dashboard's Plan card buys an upgrade through checkout and explains a refusal instead of alerting a raw 403 (691cf48a67)

`f00b3ebe32` made raising a plan a purchase or an operator grant: `PUT /v1/teams/:slug/plan` keeps or
lowers a tier for an owner or admin and `apply_plan_everywhere` answers a raise 403. The Plan &
Enterprise card (`team-privacy-settings-client.tsx`) still sent every tier through that route and
`alert()`ed the raw body, so a non-operator owner's Pro and Enterprise buttons ended in a browser
alert reading "Error: raising the plan to 'pro' needs a paid checkout (POST /v1/billing/checkout) or
a platform operator". `ui/lib/plan-change.ts` now routes by authority (`planAction`): the current tier
does nothing, an operator (`usePlatformAdmin()` true) and any lower tier use the direct PUT, and a
raise by anyone else (operator status false or unknown) opens the Billing page's own checkout,
`POST /v1/billing/checkout {kind: "plan", plan}`, and follows its `url` (Stripe's page, or the mock
checkout; `applied` means a free plan took effect). A failure is worded by `planRefusal`: a 403 is
explained (a higher plan is bought on the Billing page, only a platform operator can change a plan
without paying), the node's own sentences (billing is not set up, the provider could not open the
checkout) pass through, and a raw request line, markup or anything long never does; the card shows it
inline with an Open Billing link and a team with no plan record (a personal account) shows plain
disabled tiers and the link instead of buttons that 404. `apiSend` now throws `ApiError(status, text)`
so a caller can tell a 403 from a 502; its message is byte-identical, so no other caller changes.
Witnessed on a real enforced throwaway node behind `next dev`, headless Chrome over CDP: before, a
non-operator owner's click on Pro or Enterprise sent two PUTs (the second is the re-mint retry), both
403, and raised the alert; after, it sends one checkout request, and since that node has no Stripe key
the node answers 503 "billing is not set up on this platform ... nothing was purchased or charged",
which the card shows with the Billing link (no PUT, no alert, plan unchanged); downgrades (pro to
hobby, enterprise to pro) are PUT 200 and update the card; a stale operator claim gets the node's 403
and the explanation; an operator session switches hobby to enterprise directly (PUT 200, no
checkout). On an unenforced mock-billing node with no session mint (operator unknown) Upgrade to Pro
opens the checkout (200), the browser follows `/billing/checkout?session=...`, Pay confirms and
`/billing?success=1` shows team and billing on pro. Stripe's hosted leg cannot run offline
(`billing::stripe_checkout` calls api.stripe.com directly) and was not exercised. On an unenforced dev
node without a session mint a raise now opens the mock checkout instead of switching instantly.
Not rolled.

## 2026-10-13 — a model download never blocks the reconcile loop, is bounded in time and disk, and leaves no partial or orphaned file (c43d864248)

`fluid.json`'s `inference.model` is tenant input that reaches a real network fetch, and `reconcile_local`
awaited `ensure_model` inline in the serial reconcile loop. One slow-drip URL (a byte every few seconds
never trips a stall and never ends) stopped every other project's endpoint on the coordinator; the
download had no time bound; the only size bound was the 200 GiB cap, with no check that the volume could
hold it; and the partial file leaked on an error mid-transfer while the retired `fnv(url)`-named blobs and
the content blobs no pointer names were never reclaimed. `inference::model_state` now never awaits a
download: a cached blob returns at once, otherwise the download runs in its own task (one per URL, shared
by every project naming it; a failed URL is remembered for 300 s) and the loop moves on. `ensure_model` is
bounded in time (`HIVE_INFERENCE_MODEL_STALL_SECS` 60 s without a byte, which also bounds connect and
headers; `HIVE_INFERENCE_MODEL_TOTAL_SECS` 2 h) and in disk (`HIVE_INFERENCE_MODEL_DISK_RESERVE_BYTES`
2 GiB kept free, checked against the declared length up front and every 256 MiB), its partial file is a
Drop guard deleted on every exit, and `sweep_orphans` removes, on every download, partial files untouched
for 15 minutes plus retired `hive-<16 hex>.gguf` blobs and unreferenced `<64 hex>.gguf` blobs older than an
hour (never a blob a `*.ref.json` names). Witnessed on a real node (`HIVE_GPUS` so it coordinates, five
projects deployed from `file://` repos naming models on a hostile loopback host, orphans seeded;
`scratch2/work/secfix-model.sh`): a slow drip blocked the loop before (3 of 4 healthy projects never got a
status) and ended at the 40 s total limit after with every healthy project advanced on the first tick; a
stalled transfer never ended before and failed at 6 s after; a declared length above the free space started
writing before and was refused at once after; a connection reset at 200 KB left a 200000-byte `.part`
before and nothing after; the seeded orphans were untouched before and removed after. Operator note: a
model larger than the free space minus 2 GiB now fails its own endpoint ("insufficient disk space for the
model") instead of filling the volume, and legacy `hive-<16 hex>.gguf` files in `HIVE_MODELS_DIR` older
than an hour are deleted (nothing has read them since the cache became content-addressed). Not rolled.

## 2026-10-13 — the pre-roll key audit repeats the boot parser; a set-but-empty HIVE_SECRET_KEY is named at boot and failed by the audit (d059b1f297)

`scripts/audit-secret-keys.sh` approximated `secrets.rs::hex_to_32` instead of repeating it. A
`HIVE_SECRET_KEY=` line (set but EMPTY, which the node refuses to boot on) was reported as absent and
passed; an uppercase, mixed-case or whitespace-padded 64-hex value (which the node boots on) failed; the
check script was written as root to a predictable path under `/tmp` on the audited host; an ssh session
that died after the first lines read as a clean host; and when systemd gave no MainPID the script audited
whichever `pgrep -x hive-cloud` returned first (fc-orlando also runs DevHub's hive-cloud), so a stopped or
crash-looping hive-node passed. The key check is now exactly what the boot parser accepts, a set-but-empty
value is a FAIL worded like the boot error (which now says "set but EMPTY (a `HIVE_SECRET_KEY=` line with
no value)"), the process is `hive-node.service`'s MainPID and no MainPID is a FAIL, the check travels as
`sh -c "<script>"` (no file on the host), and every host's output ends with an `END` sentinel whose
absence is a FAIL. Witnessed against a real hive-cloud boot per input (`scratch2/work/secfix-audit3.sh`):
of 11 values the old script disagreed with the node on 5 (EMPTY passed; upper, mixed, padded and `+digit`
failed), the new one on 0; over a stub ssh/systemctl/pgrep and a fake `/proc` an empty key on the real
MainPID, a stopped service beside a DevHub hive-cloud and an output cut after one line were each `clean`
(exit 0) before and FAIL (exit 1) after. Operator note: an `Environment=HIVE_SECRET_KEY=` line in a unit
or drop-in has always stopped the node at boot and is now a FAIL in the audit too (unset it or give it 64
hex characters); a value the audit used to reject for case or padding is no longer a finding. Not rolled.

## 2026-10-13 — a queue consumer's delivery URL is built from the one label its ownership check resolved (f05c7a0540)

`consumer_target_owned` validated and resolved only the first dot-label of a consumer's `script_name`,
while `CloudState::deploy_url` formats the WHOLE alias under `HIVE_INGRESS=ngrok` and the push loop
handed it the tenant's whole string, then POSTed through the unfiltered internal client.
`own-app.localhost:PORT`, `own-app.x@localhost:PORT` and even `own-app.localhost` (valid labels only, no
port) registered as the tenant's own app and the platform connected to whatever followed the first label.
`queues::delivery_label` (the first label, lowercased) is now the one value the ownership check resolves
and the delivery URL is built from, and every label is validated at registration so a malformed tail is
refused instead of silently ignored. Witnessed on a real node with ngrok ingress and TLS ClientHello spies
on loopback (`scratch2/work/secfix-queue-ngrok.sh`): before, all three registered 200 and the platform
opened 2 connections to the spy for each; with label validation alone the valid-label tail still
connected; after, the port and userinfo tails are 403, the valid-label tail registers but nothing reaches
the host it names, and `own-app` and `own-app.shadw.app` still register (a dns-ingress node still
delivers to the tenant's own app). Operator note: a consumer registered earlier with a dotted
`script_name` now delivers to its first label only (`HIVE_INGRESS=ngrok` is retired fleet-wide, and dns
ingress already ignored the tail). Not changed: `CloudState::deploy_url` still formats the whole alias
under ngrok for its other callers (gateway-generated single-label aliases; `world::deliver_orphan_job`
passes a project NAME, and a project named `own-app.attacker.example` can be claimed, but that request
goes through the `hive-outbound` guarded client). Not rolled.

## 2026-10-13 — the DB gateway bounds a connection before it is established, never while it is (c85a293c54)

The accept bounding of the previous wave spliced the two streams with a `select!` over two copies and a
10-minute no-bytes limit. An ESTABLISHED session was cut after 10 idle minutes (a pooled idle
Postgres/Redis connection, `LISTEN`/`SUBSCRIBE` and a long statement all carry nothing for hours), the
first direction to finish ended the other (a client that half-closed after its request lost the reply),
the backend was connected right after the handshake (a client that handshook and then sent nothing held a
Postgres connection slot at no cost), and the pending cap was global only (one source address could fill
it and lock every other client out). The preamble read, the TLS handshake, the first client byte and the
backend connect now run under `HIVE_DB_GATEWAY_ACCEPT_DEADLINE_MS`, the first byte is read before the
backend is touched, and a `PendingGuard` (global `HIVE_DB_GATEWAY_PENDING_MAX` 512 plus per source
`HIVE_DB_GATEWAY_PENDING_PER_IP_MAX` 32, an IPv6 source counted as its /64) is released once that byte is
forwarded. An established session has no idle bound at all: TCP keepalive (60 s + 15 s) finds a vanished
peer and `copy_bidirectional` copies each direction on its own, so a half-closing client still gets the
whole reply and a `close_notify`. Witnessed on a real node with a seeded `*.db.witness.test` bundle (a real
TLS handshake, SNI routing to a real database record, a stand-in engine on its `local_port`;
`scratch2/work/secfix-dbgw2.sh`): an idle established session was dropped after 600.2 s before and was
alive after 660 s idle with a second round trip after; a half-closed request got no reply before and the
whole reply plus a clean `close_notify` after; a silent post-handshake client cost the engine 1 connection
before and 0 after; with a global cap of 6 and a per-source cap of 4, one address's 10 silent connections
locked a second address out before and left it connecting after (4 held, 6 closed at once). Operator note:
`HIVE_DB_GATEWAY_IDLE_MS` no longer exists; the keepalive options themselves are not proven live (a vanished
peer cannot be simulated on loopback). Not rolled.

## 2026-10-13 — a team role floor on a mutation honors an unenforced node (971ed250ae)

The role floor `a705fe33c3` put on project env put/delete and API-key revoke was `require_team(...)`, which
has no unenforced escape: a dev node (no `HIVE_JWT_SECRET`) has only anonymous callers, so those three
calls went from 200 to 403, while every other role gate (`operator_allowed`, `require_auth_read`) lets an
unenforced node stay as open as it is for every other mutation. `admin::require_team_floor` wraps it: with
`auth::enforced()` false it returns the caller's tenant and applies no floor; with auth enforced it is
exactly `require_team`. Witnessed on real throwaway nodes (`scratch2/work/secfix-devteam.sh`): dev node,
anonymous env put, env delete and key revoke 403/403/403 before and 200/200/200 after; enforced node
identical before and after (anonymous 401; viewer 403 on env put, env delete and key revoke; member 403 on
key revoke, 200 on env put; admin 200 on key revoke). Not rolled.

## 2026-10-13 — world reads take the loopback sidecar endpoint from the project's own database record, never from tenant env (bef14c9026)

The world client's loopback exemption (`c6bcee01ed`, so a node can reach its own managed Redis REST shim)
accepted ANY loopback `http://127.0.0.1:PORT` a project's `UPSTASH_REDIS_REST_URL`/`WORKFLOW_REDIS_REST_URL`
named, so a tenant could aim the node's world client at any other service on the node's loopback.
`world::world_config` now accepts a loopback URL only when it equals
`DatabaseStore::local_redis_rest_url(project, team, node)`: the REST URL of THIS project's own Redis
record, same tenant, hosted on this node (`is_platform_loopback_shim` stays as a shape check, never the
gate). Witnessed on a real node with a recorder on the managed database's port and another on an arbitrary
loopback port (`scratch2/work/secfix-worldshim.sh`): before, a tenant-set `http://127.0.0.1:9582` received
the world client's POST (ZRANGE with the tenant-chosen bearer); after, 0 requests, while the managed
database's own endpoint is still reached (1 request before and after). Not rolled.

## 2026-10-13 — an amount from a request body can no longer wrap into a credit: every cents input is bounded (0342ebc567)

`POST /v1/billing/charge` debits the caller's own account by `cents`, a `u64` straight from the request
body, and `BillingStore::charge` cast it `as i64`. Above `i64::MAX` the cast is negative, the balance
check passed and `balance -= negative` CREDITED the account, so any tenant minted credits:
`{"cents":18445744073709551616}` took a hobby account from 4000 to 1000000000004000 cents. Amounts around
`i64::MAX` panicked the handler ("attempt to subtract with overflow"; the release profile has overflow
checks off, so a release build wraps silently). Amounts are now bounded by `billing::MAX_AMOUNT_CENTS`
($10 billion) at every entry point that takes one: `charge()` returns the typed `ChargeError::OutOfRange`
(the route answers 400; insufficient credits stays 402 with its text), a credits checkout and an operator
grant answer 400 above it, `add_credits` converts with `i64::try_from` and every balance update saturates.
A refused charge no longer leaves the allowance it had started to consume (the decision is made before the
account is touched: `used_cents` 100 -> 500 before, 100 -> 100 after), and a credits checkout of 0 cents
answers its own 400 instead of falling into the free-plan branch. Witnessed on a real enforced node
(`scratch2/work/secfix2-billing-charge.sh`): before, 10 defects (credit minted by 2^64-10^15 and u64-max,
panics on 2^63, one cent past the bound accepted, an unpayable 4.6e18 debt accepted, a credits checkout and
an operator grant above the bound applied, a refused charge consuming the allowance); after, none: 400
with the balance unchanged, a charge at exactly the bound still applies, and normal charges, charges
against prepaid credits and operator grants within the bound are unchanged. The route has no caller in
this repository (no dashboard, CLI or SDK uses it): retiring it is the owner's call. Not rolled.

## 2026-10-13 — a checkout is applied only when a payment stands behind it: Stripe failing no longer hands out a mock session (fb3b773810)

`billing_checkout` opened a real Stripe Checkout when `STRIPE_SECRET_KEY` was set and, when Stripe
errored (an amount over its limit, an outage), logged "falling back to mock" and returned a MOCK
session; with no key it always did, on any node. `billing_confirm` applied any checkout whose
`stripe_session_id` was empty without verifying a payment. So any tenant bought credits or a plan for
nothing: credits of $1,000,000.00 (an amount Stripe refuses, so the fallback fired) and the enterprise
plan, on an enforced platform with a Stripe key and on one without. The rule now lives where a purchase
becomes applied, `BillingStore::confirm_checkout(id, Payment)`: `Payment::Stripe` (a session verified
with Stripe, or a signature-verified webhook) or `Payment::Unproven`, which completes an amount due only
where `billing::mock_checkout_allowed()` (no Stripe key AND no `HIVE_JWT_SECRET`: a developer's node),
else `Unconfirmed::Unpaid` (402) without consuming the record, so a mock checkout persisted or replicated
from a dev node cannot be spent on a platform either. `billing_checkout` answers 503 on an enforced node
with no Stripe key (before any record opens) and 502 plus a discarded record when Stripe cannot open
one. Witnessed on real nodes (`scratch2/work/secfix2-billing-mock.sh`: {enforced, dev} x {Stripe key set
but unreachable behind a dead HTTPS proxy, no key}, plus a dev checkout persisted and confirmed after a
restart as an enforced platform): before, 13 defects (credits and plan purchases applied with no payment
in three of the four combinations, a leftover mock checkout spent: balance 0 -> 7777); after, none: 502,
503 or 402 with balance and plan unchanged and no open checkout left behind, while the dev flow (no key,
auth not enforced) still credits and switches plans and a free hobby checkout still applies. The Stripe
success path needs a live Stripe account and is unchanged. Operator note: an enforced platform with no
`STRIPE_SECRET_KEY` now refuses paid checkouts (503) where it used to grant them; set the key, or grant
through `POST /v1/admin/billing/grant`. Not rolled.

## 2026-10-13 — a plan is bought or granted, never self-assigned: the tier-change primitive decides who may raise it (f00b3ebe32)

`PUT /v1/teams/:slug/plan` was gated only by `require_team(owner|admin)`, so a team's owner, an admin,
or an owner's API key could set the team's plan to `enterprise` (or `pro`) with no checkout: the team
record and the billing account both read the paid tier and every plan-gated feature (IP blocking, SIEM,
SAML, SCIM, fail-over, 1-hour runtimes) opened for free. `team_create` already encoded the policy
("create the team on Hobby, then complete checkout to upgrade"), but only at that one route. The check
now lives at the sink every tier change funnels through: `admin::apply_plan_everywhere` takes a required
`PlanChange` (`Request(claims)`: an operator, or anyone on an unenforced dev node, sets any tier and
everyone else may only keep or lower it; `Paid`: a verified checkout, or the mock checkout of a node
with no Stripe; `Cancellation`: the Stripe subscription-deleted downgrade; `AdminTenant`: the platform
admin's own enterprise lock), refuses an unknown plan (400) and a raise without the authority to raise
(403 naming the checkout route). All eight call sites pass their reason. Witnessed on a real enforced
node (`scratch2/work/secfix2-plan.sh`): before, the owner, an admin and the owner's API key all got 200
with the plan changed and the enterprise-only IP blocking then answered 200; after, all 403 with the plan
unchanged, while the operator's PUT, an owner downgrade, billing grants, the free-plan checkout, a mock
pro checkout plus confirm, an operator creating a pro team and a non-operator's 402 on a paid team are
unchanged. Operator note: the dashboard's Plan card and `shadw team plan` still call this route, so for a
non-operator their upgrade buttons now answer 403 until the card routes upgrades through
`POST /v1/billing/checkout`; downgrades work. Not rolled.

## 2026-10-13 — a restart no longer swaps the host-less default deployment for the oldest one (c96d546a79)

A request with no Host header on the public listener is answered by the gateway's default deployment.
Live, `deploy` and `promote` make the deployment that takes a project's alias the default (the
newest); `Gateway::restore` did `default.get_or_insert(id)` while `persist::restore` replays records
oldest first, so after every restart the oldest ready record (any tenant's) answered instead and stayed
the default until the next deploy. `restore` now sets the default to each alias winner, ending on the
newest. Witnessed on a real node (`scratch2/work/secfix2-default.sh`): before, a host-less request
answered SITE-bravo (newest) live and SITE-alpha (oldest) after a restart; after, SITE-bravo both times.
Reviewed and NOT changed: `ProjectStore::upsert_env`/`delete_env` match on key only; the dashboard edits
and deletes by key (one row per variable, the target is a field of that row) and `DELETE /env/:key`
carries no target, so that is the data model, not a slip. Not rolled.

## 2026-10-13 — placement and the control-plane election ask the one shared reachability and address predicate (2c90d1ea2d)

`schedule::place_for_project` (lease-holder stickiness) inlined a reachability check without the
`dialable()` test `place` applies, so a lease holder with a peer id and an iroh address but no public
address, no relay, no live trunk and no admin URL was chosen, the build was dispatched to it and, since
stickiness also pins the deploy to its holder, failed "could not reach any target" though this node could
have built it. `Cluster::billing_leader_with_pref` kept a non-trimming copy of `Cluster::addressable`, so
a node advertising `public_ip` `"   "` with the lowest peer id won the identity election over a node
with a real public address. Both now call the shared predicates (`schedule::reachable`,
`Cluster::addressable`, and `schedule::has_public_address` delegates to the latter). Witnessed on a real
enforced node pulling crafted registry entries and a lease from a fake HTTP gossip peer
(`scratch2/work/secfix2-place.sh`): before, a git deploy was placed on the unreachable holder and ended in
error and `/v1/cluster` named the blank-address node leader; after, the build is ready on this node and
the leader is the node with the real address. Not rolled.

## 2026-10-13 — SCIM authenticates by its own bearer on an enforced platform, and the SAML ACS no longer lets a signed-in stranger forge a roster member (c45cbe1508)

SCIM (`/v1/scim/v2/*`) authenticates every request by the team's own bearer but sat behind the platform
gate, so an IdP's POST/PATCH/DELETE (no platform JWT) was answered 401 before the handler. The prefix is
now on the open-route list and `scim_team_for_token` fails closed (empty token, and a stored value no key
can open is never compared: `secrets::decrypt` hands such a ciphertext back unchanged, which made the
leaked ciphertext a working bearer). The open-route list is one function, `auth::is_open_route`, used by
`require_auth` and by `admin_ingress`, whose private copy had drifted (it lacked `/v1/billing/webhook`,
`/v1/browser/admissions` and `/v1/zkauth/project-team`, so an unauthenticated POST to those was refused 401
at the api ingress though the node treats them as self-authenticating; they now reach their handlers, which
verify the Stripe signature, the admission's claims and the internal secret). The SAML ACS verifies no
signature and is NOT opened, but any signed-in caller of any team could POST a forged NameID to another
team's ACS and, with `auto_provision`, add that email to its roster: it now requires the path team's
owner or admin (anonymous stays 401, another team's caller or a plain member is 403), and `RelayState`
goes through `auth::same_origin_path` (it was appended to the dashboard URL as given:
`https://dash.example` + `@evil.example/x` is host evil.example). Witnessed on a real enforced node
(`scratch2/work/secfix2-saml-scim.sh`): before, another team's owner and a member forged roster entries,
4 of 6 RelayState values left the dashboard host and SCIM POST with the IdP bearer was 401; after, 403,
every RelayState lands on the dashboard, SCIM create/list/PATCH/DELETE work (also through the ingress) and
empty, wrong, absent, JWT or other-team bearers provision nothing. Operator note: SAML sign-in still
cannot complete on an enforced platform (an IdP's browser POST carries no platform JWT) until assertion
signatures are verified; the ingress now lets the three routes above through to their handlers. Not rolled.

## 2026-10-13 — a DNS-only domain record no longer invents registrar facts the platform never measured (f60ae96f5f)

`dns::default_domain` seeded every record with registrar "OpenEdge", renewal_price "$13 per year",
auto_renew true and an expiry one year out; `GET /v1/domains/:domain` served them as fact (the dashboard's
Expiration Date of an attached domain still showed the invented date) and `PUT .../auto-renew` flipped a
flag that renewed nothing. The four keys now carry no value (`""`, `""`, `false`, `0`) and
`DomainStore::load` scrubs them on every boot and snapshot adoption, so records persisted or replicated by
an earlier build stop serving them. The keys are emptied, not removed: a node on an earlier build parses
`state.json` and replicated snapshots with them required and a failed parse resets that node's whole
platform state, so removing one is a rollback and mixed-fleet hazard. The dead auto-renew route and its
setter are gone (404). Witnessed on a real node (`scratch2/work/secfix2-dns.sh`): before, a fresh record
read OpenEdge / $13 per year / true / +365d, the toggle answered 200 and a persisted record kept serving
the invented values after a restart; after, empty, false, 0, a 404, scrubbed on load, and the older binary
still boots on the data dir the new one wrote. The dashboard stops rendering them in `6ef9c7427b`
(`DomainRecord` no longer declares the keys; the attached domain's Expiration Date row and the list's
"auto-renews" are gone), witnessed in headless Chrome against throwaway nodes: roll the dashboard BEFORE or
WITH the nodes, because an old dashboard against an upgraded node renders an epoch-zero expiry ("Dec 31,
1969") for an attached domain and still prints "auto-renews". Not rolled.

## 2026-10-13 — a checkout names a real plan, and deleting a database that does not exist is a 404 that plants no tombstone (71f14d7300)

`billing_checkout`'s free-plan shortcut resolved the requested plan through `plan_spec`, which falls back
to Hobby for any string, so `"garbage"`, `"Enterprise"` or `"hobby "` looked free and the raw string was
stored in the team record while billing normalized to hobby (the two halves disagreed about the tier).
`billing::known_plan` is the lookup without the fallback; the checkout and both lookups in `billing_grant`
use it, so a request naming no plan is a 400 listing the plans. `database_delete` retried the lookup for
replication lag and then, for an id still unknown, called `remove_db_and_purge_data` and answered
`{removed: id}`; `remove_db` inserts a tombstone for the id, which replicates fleet-wide and drops whichever
record that id later names, another tenant's included. An id still unknown after the retries is now a 404
and nothing is removed or tombstoned. Witnessed on a real enforced node (`scratch2/work/secfix2-billing-db.sh`):
before, four garbage plans answered 200 with the team record reading that string and a delete of
`db_0123456789abcdef` answered 200 with a tombstone in `state.json`; after, 400 and 404 with neither, and
every legitimate checkout, grant and delete behaves as before. Operator note: a retried delete of a database
that is already gone now answers 404. Not rolled.

## 2026-10-13 — the audit log appends off the serving worker, can no longer interleave lines, and boots from its tail (13926dc5f3)

`AuditLog::record` ran `create_dir_all`, open, `writeln!` and `sync_all` on whichever tokio worker served the
mutation, so a burst of mutations parked every worker in an fsync; concurrent appends interleaved (`writeln!`
on an unbuffered file is two writes, line then newline), so in a 96-mutation burst 22 lines of `audit.jsonl`
held two JSON objects and 34 were blank; and `AuditLog::new` read the whole file at boot to keep 1000 lines.
`record` now assigns the sequence number and queues the line under the ring lock (file order is sequence
order) and never waits; one writer thread appends everything queued under a single fsync (group commit,
bounded by `HIVE_AUDIT_QUEUE_MAX`, default 65536, then the entry stays in the ring only and is counted and
logged); `audit::flush_blocking` drains it at graceful shutdown (first thing in `persist::flush_blocking`);
boot reads only the file tail (64 KiB chunks, at most 8 MiB). Line format, field order and the ring API are
unchanged. Witnessed on a real enforced node (`scratch2/work/secfix2-audit.sh`): 150 sequential mutations p50
8.0 ms -> 0.3 ms; 96 concurrent mutations 0.25 s -> 0.03 s with an unrelated `GET /healthz` p99 69.8 ms ->
3.2 ms; the burst file went from 22 merged and 34 blank lines to 247 parseable contiguous ones; boot over a
212 MB file 333 MB peak RSS (+216 MB) -> 124 MB (+1 MB), sequence continuing from the tail. Operator note: an
entry is on disk after the writer's next group commit (milliseconds), not before the mutation returns; a
SIGKILL inside that window loses those entries (the platform snapshot is already written behind the same
way). Not rolled.

## 2026-10-13 — a supervised loop's restart backoff resets after a healthy run (f52e6f0d10)

`supervise::spawn_supervised` doubled its restart backoff from 5 s to a 300 s cap on every crash and never
lowered it, so after a loop had crashed seven times in its life (however far apart) every later crash waited
the full 5 minutes before the subsystem restarted: one transient panic after days of running left a
leader-only job, the reconciler or a replication loop dead for 5 minutes with `/healthz` green. A run longer
than the longest backoff (300 s, on the runtime clock) is healthy, so the crash that ends it starts a new
incident: the backoff returns to 5 s. A deterministic crash loop still doubles 5, 10, 20 ... 300. Witnessed
with a throwaway crate outside the repo that includes the real `supervise.rs` (`scratch2/work/s2-sup-witness`),
on tokio's paused clock and on the wall clock: before, the restart after a 400 s healthy run waited 300 s;
after, 5 s, then 10 s and 20 s for immediate re-crashes. Not rolled.

## 2026-10-13 — the password gate no longer reflects the visitor's path into its form or redirect, and the edge forward keeps `//` paths (1f5458f68f)

`edge::password_page` interpolated the visitor's raw request path, unescaped, into `<form action="{path}">`.
Confirmed live: a request for `//evil.example/steal` made the gate submit the deployment password a visitor types
to evil.example (the attacker needs only the deployment's URL, not the password), and a path containing a double
quote, which the request-line parser tolerates, broke out of the attribute (`/x"onfocusin="alert(document.domain)`
landed an event handler on the form, fired by the password field's autofocus: a reflected XSS on the protected
deployment's own origin). After a correct password the unlock redirect used the raw path as its `Location`, so
`//evil.example/x?__shadw_pw=...` redirected protocol-relative. The form now has no `action` (an absent one submits
to the current URL), the project name is HTML-escaped, and the redirect goes through `auth::same_origin_path`.
Also fixed: my earlier forward-leg change (fe27627bd2) resolved the visitor's path with `Url::join`, which treats a
`//host/..` path as a network-path reference, so a legitimate `//assets/x.js` request had every candidate refused;
the forward leg and `spawn_revalidate` now set the target as the PATH and QUERY of the gateway URL
(`with_request_target`), identical to the old form for every legitimate target and unable to move the authority.
Witnessed on throwaway enforced nodes (`secfix-pgate.sh`, `secfix-edge-redirect2.sh` on a real two-node mesh):
before, the form action, the injected attribute and the protocol-relative redirect all worked; after, none do and
the legitimate unlock flow is unchanged. Operator note: a visitor requesting a protected deployment at a `//`-prefixed
path that then submits the password lands on the site root instead of that path. Not rolled.

## 2026-10-13 — `/api/tv/hls` decodes raw deflate, and an empty body under a content-coding is an empty body

Two regressions in the content-coding decoder added a few commits earlier, from the round-3
adversarial review. `deflate` was decoded as zlib-wrapped only, so an origin that sends raw RFC
1951 deflate under `Content-Encoding: deflate` (undici and browsers accept it by sniffing the
first byte) failed with "incorrect header check" after the 200 headers: a stream error with no
bytes on the segment lane, a 502 on the playlist and key lanes. And an empty body under a coding
(a 204, `content-length: 0`, a chunked body with no chunks) ended in `Z_BUF_ERROR` "unexpected
end of file" instead of being an empty body. `safe-fetch.ts` now makes the decoder on the first
non-empty chunk (`LazyDecoder`): a deflate body is zlib-wrapped when `(firstByte & 0x0f) === 8`
and raw otherwise, and a stream that never carries a byte is an empty body (one rule for 204,
`content-length: 0` and chunked-empty, no separate status check; an incomplete stream still
errors). The decoder is paused while the wrapper's readable side is full, so it still never runs
ahead of the reader, destroying the wrapper destroys it, and every cap (encoded and decoded) is
unchanged. `role=key` stays client-supplied and unsigned on purpose, and the route's contract
line now says so: a caller can add it to any proxy URL to relabel at most 4096 bytes of a public
body as inert `application/octet-stream` (`nosniff`, `no-store`) past the media-type allowlist,
which any upstream that labels its bytes `application/octet-stream` already gets at 64 MiB, so an
HMAC would only couple an anonymous relay to `HIVE_INTERNAL_TOKEN`. Witnessed with the previous
code (loaded from git, or built as a production `next start` scratch app) against this code: 71
module-level checks with real zlib streams, 13 failed before (raw deflate, empties) and none
after, including 600 random raw-deflate bodies (every level and strategy, odd chunk sizes) and
300 zlib-wrapped ones that all decode (the first byte of a raw stream was never `x8`), endless
empty stored deflate blocks and a 256 MiB raw-deflate bomb ending at the caps, and a slow reader
of a 2 GiB raw-deflate bomb that pulled 192 KiB; 15 over-the-wire empty-body checks through a
cloudflared quick tunnel (the edge strips `Content-Encoding: deflate` in every spelling, so raw
deflate can only be witnessed at module level and the wire runs use `x-gzip`), 7 failed before on
`next dev` and on a production build (a socket error on the segment lane, 502 on the playlist and
key lanes) and none after. The reviewer's "empties run to the 25 s deadline pinning a core" did
not reproduce in either version: empties end in under a second at under 100 ms of server CPU, and
a body that stalls after its headers ends at about 20 s (two 10 s idle timeouts) at 20 to 50 ms
of CPU, plain or encoded alike. The 47-row hostile-URL table (0 recorded connections), the
10-row key table, 53 of 53 bound checks, 27 of 27 encoding checks and real hls.js playing six
AES-128 variants all hold. Not rolled.

## 2026-10-13 — mesh-RPC team scoping, sandbox cap/isolation, API-key role gates, project_preview redirects

Four findings from an adversarial review landed together in admin.rs (a705fe33c3): `require_mesh_rpc(.., None)`
accepted a mesh-internal delegation minted for ANY tenant's own team on `node_announce_public`/`push_sms_relay`/
`push_sms_direct_mx` (fleet-wide RPCs with no per-tenant meaning) — now `Some("mesh")`, no real sender affected.
`/v1/sandbox` now refuses outright (503) on a Mock backend, which previously ran every command on the HOST's own
`/bin/sh` with zero isolation; separately its per-tenant cap was released by the HTTP request's Drop guard, not the
submitted job, so six dropped connections left six live cells while the cap still showed room — the slot now moves
into a detached watcher tied to the job's own terminal state. `apikey_revoke` and `project_env_put`/`_delete` had no
role gate beyond tenancy (a bare viewer could revoke the team's owner key or write/clear env vars); both now call
`require_team` with an explicit floor. `project_preview` now fetches through `same_origin_client()` instead of the
bare 10-redirect client, so a tenant's own previewed app can no longer redirect that server-side fetch to an
internal address and have the body echoed back as "preview". Witnessed live on a throwaway auth-enforced node for
all four; operator note: a viewer/member key doing any of these now gets 403/503 instead of succeeding.

## 2026-10-13 — workflow/cron step invoker no longer lets a step path redirect to an arbitrary host (1ddd73eaab)

`main.rs::invoke` built its request URL as `format!("{public_base}{path}")` with `path` a tenant-controlled workflow
step field; a step path `@127.0.0.1:PORT/x` made the node GET an internal listener with the body returned in the run
output (confirmed live). Fixed via `Url::join` plus an explicit same-authority check, over `same_origin_client()`.

## 2026-10-13 — outbound guard closes a SIIT address gap and refuses its own dev escape on an enforced node (cc55b1a4f5)

`is_public`'s IPv6 handling recognized standard v4-mapped and NAT64 embeddings but not SIIT's `::ffff:0:a.b.c.d`
form, which read as public regardless of the embedded v4 address. `HIVE_OUTBOUND_ALLOW_PRIVATE=1` is now refused
outright on a node with auth enforced (mirrors `HIVE_JWT_SECRET` set) and logs a boot WARN either way.

## 2026-10-13 — web-push endpoint allow-list parses the URL instead of hand-splitting it (66873f7b1f)

`allowed_push_host` read everything before the first `:` after `://` as the host, which is exactly what userinfo
looks like: `https://fcm.googleapis.com:x@127.0.0.1:PORT/ep` passed the allow-list while dialing the attacker's own
loopback listener (confirmed live). Now parses with the real URL parser and refuses any userinfo outright; the send
path also re-validates through `hive_outbound` at send time instead of trusting the subscribe-time check alone.

## 2026-10-13 — edge forward never follows a redirect and no longer lets a visitor's request path redirect the candidate dial (fe27627bd2)

The HTTP-direct forward leg and `spawn_revalidate` used the bare 10-redirect shared client, so a deployment
candidate's 3xx was followed server-side (SSRF) instead of relayed to the visitor's own browser; both now use
`hive_outbound::internal_client()`/`same_origin_client()`. The forward leg's URL was also `format!`-built from the
visitor's own request path, vulnerable to the same userinfo/authority-override class the queue-consumer and
cron/workflow-step fixes close — rebuilt via `Url::join` plus a same-authority check.

## 2026-10-13 — world-redis cmd/pipeline route through the outbound guard without breaking the platform's own managed-redis sidecar (c6bcee01ed)

`cmd`/`pipeline` dialed a tenant-settable Redis-REST URL through a bare client with no guard at all. Now resolved
through `resolve_world_endpoint`: the platform's own `provision_redis` loopback shim (exact shape match) goes over
`internal_client()`, anything else through the full `hive_outbound` guard — fixing what would otherwise have been a
regression (the guard alone would refuse the platform's own loopback http sidecar). `deliver_orphan_job` now reuses
the hardened `deploy_url` instead of hand-building its host string.

## 2026-10-13 — inference model download goes through the outbound guard, a size cap, a generic error, and a content-addressed cache name (20cd9a7ad2)

`ensure_model` downloaded a tenant-controlled `fluid.json` model ref through an unguarded client with no size bound
and an error that echoed the raw upstream status back to the caller (an oracle for internal addresses). Now
`hive_outbound::validate_fetch_url`/`following_client()`, a `HIVE_INFERENCE_MAX_MODEL_BYTES` cap (default 200 GiB)
checked against both `Content-Length` and the running byte count, two fixed generic error strings, and a cache
filename keyed by the BLAKE3 digest of the downloaded bytes themselves (hashed incrementally) rather than an FNV
hash of the URL, which was not collision-resistant.

## 2026-10-13 — `shadw keys create` defaults to a role every team role can mint (dc79d6746a)

The CLI's `--role` default was "owner", which 403'd for every member/admin caller once the API-key role matrix
started enforcing "no higher than the caller's own role" (ccd6714edb). Default changed to "member".

## 2026-10-13 — the preview unlock binds the proof's team to the project's owner and `next` cannot leave the origin (f8ad2eadbb)

`zkauth::bootstrap` (`GET /_shadw/zk` on a preview host) verified a membership proof against whichever
team ring the visitor-supplied `t` named and never compared `t` with the project's owner, so a proof
minted over any team's roster for scope `preview:<project>` unlocked that project's private preview
(302 plus the `shadw_zk` cookie); the dashboard's `/api/preview-unlock` was the only enforcement. Its
`next` filter (`starts_with('/') && !starts_with("//")`) also admitted `/\evil.example` and
`/<TAB>/evil.example`, which the WHATWG URL parser resolves to `https://evil.example/` (an open
redirect on the trusted preview host). Now `bootstrap` requires `t` to be the ring allowed to unlock
the project, taken from the node's own state (`zkauth::local_owning_team`: the settings row, else the
newest deployment record's tenant, one function shared with `/v1/zkauth/project-team`; an UNOWNED
project unlocks only from the legacy `personal` ring, the dashboard's own mapping) and answers the same
opaque 401 as a bad proof, and `next` goes through `auth::same_origin_path`, the Rust twin of
`ui/lib/same-origin-path.ts`. Witnessed on a real enforced throwaway node
(`scratch2/work/secfix2-zkauth.sh`, proofs minted through the internal endpoints exactly as the
dashboard route does): before, 7 attacks worked (a proof over the attacker team's ring unlocked the
victim's preview and its content was served; three hostile `next` values redirected off-origin; the
attacker ring unlocked a project whose owner lives only in the deployment record and an unowned one;
the victim ring unlocked the unowned one); after, 0, while a same-team proof, the legacy personal
unlock of an unowned project, deep links with a query string, the cookie and the no-`next` default
still work, and a `next` carrying LF/CR now falls back to `/` instead of answering 500. Operator note:
the relational mirror is deliberately not consulted on this unauthenticated path, so a project just
transferred between teams whose new owner has not reached the serving node's settings row fails closed
(401) until it does. Not rolled.

## 2026-10-13 — a queue consumer's script_name can no longer redirect delivery to another tenant's app or an arbitrary host

An independent adversarial review found a live bypass of the same-session queue-consumer fix
(`3ed2e2adea`): `consumer_target_owned` checked the first dot-label of `script_name` as a project
name, but the real delivery URL was built by `format!("https://{sub}.{apps_domain}")` with `sub`
unescaped. `script_name:"x@victim-app"` has no dot, so the check saw one opaque unowned label and
let it through (the optimistic pass for an unrecognized name), while the raw string concatenation
turned the embedded `@` into userinfo and "victim-app" into the real destination host — the node
dialed tenant B's app. `script_name:"localhost:PORT#"` similarly let a literal `:` change the port
and `#` swallow the rest as a fragment, dialing an arbitrary host:port. `valid_script_label` now
whitelists `[a-z0-9-]` for the label, refusing anything else outright; `consumer_target_owned`
requires positive evidence (a settings-row or deployment-record match) instead of an optimistic
pass for an unrecognized name; `state.rs`'s `deploy_url` is hardened at the shared primitive
(`host_scoped_https_url`: builds the host through `Url::set_host`, which runs the WHATWG host
parser and rejects `@`/`:`/`#`/`/`, instead of string interpolation) so every other caller gets
this for free; delivery goes over the no-redirect internal client; the batch signature now covers
`timestamp.nonce.body` so a captured batch cannot be replayed. Witnessed on a throwaway
auth-enforced node (`scratch2/work/secrev/t_queue.sh`): before, both attacks registered (200) and
opened real connections to the victim's app / an arbitrary host; after, both are refused at
registration (403, zero connections) while the legitimate same-tenant consumer is unaffected.
Operator note: a consumer whose `script_name` is not a clean label, or does not resolve to a
project the queue's tenant owns, now registers 400/403 instead of 200; a consumer handler verifying
`x-hive-signature` must now read the new `x-hive-timestamp`/`x-hive-nonce` headers and HMAC
`timestamp.nonce.body` (a wire-format change to a feature added this session, not yet rolled). Not
rolled.

## 2026-10-13 — `/api/tv/hls` decodes a gzip body an origin sends despite `accept-encoding: identity`, and a key is decided by its URI role

The route asks for `accept-encoding: identity`, but an origin that ignores it (nginx
`gzip_static always`, a CDN that gzips every playlist) answered with a `content-encoding`
the proxy never decoded: a gzip playlist failed the `#EXTM3U` check and answered 415, a gzip
AES key labelled `mpegurl` was served as the compressed blob (a wrong key: hls.js died on
`fragParsingError` instead of an error that names the cause) and a gzip segment streamed
compressed under the origin's wire `content-length`. A key a server labels `text/plain` or
`text/html` also answered 415 (the previous fix covered only a key labelled as a playlist).
`safe-fetch.ts` `boundedSource`, behind `readBounded` and `boundedBody`, now decodes ONE
`gzip`, `x-gzip` or `deflate` coding with the cap on both the encoded and the decoded bytes:
a bomb ends at the decoded cap and the decoder never runs ahead of the reader (a 4 GiB bomb
pulled 258 KiB of its 4.1 MiB), while an endless run of empty gzip members or an unterminated
gzip header decodes to nothing and would never reach a decoded-only cap, so the encoded side
ends it; either tears the upstream down. A 206 in a coding is a 502 (a partial gzip cannot be
decoded); brotli (a 16 MiB window per request on an anonymous route), zstd and chained
codings are a 415; a decoded body is forwarded without `content-length` or range headers. The
playlist rewrite now marks the URI of every `#EXT-X-KEY` and `#EXT-X-SESSION-KEY` `&role=key`,
and a `role=key` fetch is at most 4096 bytes of `application/octet-stream` whatever the origin
labels it (empty or larger is a 502), so a key labelled `text/plain` or `text/html` plays when
it is reached through a rewritten playlist; a direct hit keeps the label rule and still
refuses it. Witnessed with a synthetic origin behind a cloudflared quick tunnel, the previous
route (HEAD) served on one port and this code on another (the quick tunnel's edge decodes
gzip, deflate and br for an `accept-encoding: identity` client and rewrites the header of a
coding it does not know, so over the wire the coding is `x-gzip`, which it leaves alone): 27
over-the-wire checks, 20 failed before and none after (the `x-gzip` playlist, key and segment,
both mislabelled keys, a 206 in a coding, 64 MiB and 256 MiB bombs, a corrupt and a truncated
stream, an endless run of empty members and an endless header, each ended by the encoded cap
with the origin connection closed); 48 module-level checks with real zlib streams (every
spelling of the coding, exact caps, damaged streams, cancel, the plain 64 MiB cut); real
hls.js in headless Chrome playing AES-128 HLS streams through the route, where a key labelled
`text/plain`, an `x-gzip` playlist and an `x-gzip` key failed before (`keyLoadError` 415,
`manifestLoadError` 415, `fragParsingError`) and all six variants play after (320x180, key
loaded, fragments parsed, no errors); the 47-row hostile-URL table (0 recorded connections),
the 10-row key table and 52 of 53 bound checks hold (the remaining one, a 64 MiB endless
stream, cannot finish inside the route's 25 s deadline at the tunnel's current 0.6 MiB/s and
ends identically on the previous route); 300 real catalog channels answer the same playlist
status before and after. Not rolled.

## 2026-10-13 — a malformed `team` for ZK enrollment is a 400, and the unlock page never calls a server fault "Access denied"

Two follow-ups from the round-2 review of the preview-unlock commits. `POST /api/zk-enroll`
passed the request body's `team` to `teamAccess` before checking it was a string, so an
object, a number, an array or `true` threw `team.startsWith is not a function` and answered
500; it now answers 400 (`team must be a string`), while an absent, null or empty `team` still
means `personal`. The unlock interstitial (`ui/app/preview-unlock/page.tsx`) rendered every
failed answer, including the retryable 503 and 502 the route sends when a Clerk or node
lookup cannot be answered, as "Access denied" with no retry and ignored `Retry-After`, so a
visitor with a valid membership was told they were refused. "Access denied" is now only a 4xx
refusal; a 5xx or no answer shows "Temporarily unavailable" and a "Try again" button that is
disabled for `Retry-After` seconds (at least 1, at most 60, 5 when absent) and re-runs the
unlock. Witnessed on the real zk-enroll handler against a throwaway node (12 request bodies:
4 threw a 500 before and none after, the 4 malformed ones are 400 and the other 8 answer as
before), and on the page in a production build of a scratch app with a stub unlock API driven
by headless Chrome (strict mode runs mount effects twice in dev): before, a 503 with
`Retry-After: 3`, a 503 without it, a 502, a 403 and a flaky 503-then-200 all showed "Access
denied" with no button; after, the 503 with `Retry-After: 3` shows "Temporarily unavailable"
and a disabled "Try again in 2s" that ignores a click until the delay has passed (one API
call, not two), then the click re-runs the unlock and the visitor enters, a 503 without the
header and the 502 count down 5 s, and the 403 still shows "Access denied" with no retry.
Not rolled.

## 2026-10-13 — the DB-gateway listener bounds every accepted connection and idles out a silent one

The Postgres/Redis wire listener (`db_gateway.rs`) spawned one unbounded task per accept with no
deadline on the preamble read, the TLS handshake or the backend connect, and no idle timeout on
the splice: a client that connects and sends nothing holds a task and fds forever, and enough of
them exhaust the node's fds, taking every other listener on the host down too. `accept_deadline()`
(`HIVE_DB_GATEWAY_ACCEPT_DEADLINE_MS`, default 15s) now wraps the preamble read, the TLS handshake
and the backend connect; a `PendingGuard` cap (`HIVE_DB_GATEWAY_PENDING_MAX`, default 512) bounds
concurrent connections still inside that window and releases the instant the backend connects, so
it never limits an established session; `splice_with_idle_timeout`
(`HIVE_DB_GATEWAY_IDLE_MS`, default 10 min) closes a spliced session carrying zero bytes in both
directions for that long. Witnessed on a throwaway node with a tightened cap/deadline
(`scratch2/work/secfix-dbgw.sh`): before, 10 silent client-held connections all stayed ESTABLISHED
indefinitely; after, only the cap's share were even accepted and all were dropped by the server
within the deadline despite the client never closing or sending a byte, while a normal client, the
unrelated admin listener and the redis wire are unaffected. Operator note: the three new env vars
are defaulted generously; raise `HIVE_DB_GATEWAY_ACCEPT_DEADLINE_MS` if a legitimate client's TLS
handshake legitimately takes longer than 15s. Not rolled.

## 2026-10-13 — at-rest key resolution fails closed instead of silently regenerating or running memory-only

`secrets::load_or_create_key` ignored a malformed `HIVE_SECRET_KEY` with a WARN and fell back to
the node's own `secret.key` file (the fleet silently stops sharing one key), regenerated an
existing `secret.key` that was not 64 hex (orphaning everything already sealed under it), and left
a memory-only key in place when the write failed (everything sealed that boot becomes unreadable
on restart, with no error anywhere). `integrity_signer::open_or_create` had the matching bug: any
read error on `integrity_signing.key` (EACCES, non-UTF-8, a directory), not just genuine absence,
was treated as "no key yet" and silently overwritten, replacing the node's established
chain-signing identity. Both now fail closed: `load_or_create_key` returns a specific `Err` for
each unsafe branch and its caller exits the process rather than hand out a key nobody agreed on;
`open_or_create` creates a fresh key only on `NotFound`, and every other read error is a loud
`anyhow` error that leaves the file untouched. The dev flow (no env, no file) is unchanged.
Witnessed on throwaway per-case nodes (`scratch2/work/secfix-keys.sh`): before, all 5 unsafe
branches fired silently; after, all 5 refuse to boot or return a loud error naming the exact file
and reason, while the dev flow, a valid `HIVE_SECRET_KEY` and a missing `integrity_signing.key`
are unchanged (3/3). Operator note: a node with an already-wrong `HIVE_SECRET_KEY` or an
already-damaged key file now refuses to start instead of silently running on the wrong key, which
is a roll hazard for a node already in that state — run `scripts/audit-secret-keys.sh` against the
fleet before rolling (read-only; flags exactly this, host by host, before any node restarts on the
new fail-closed behavior). Not rolled.

## 2026-10-13 — an HLS key a server labels as a playlist reaches the player

`/api/tv/hls` chose its playlist lane from the URL (`.m3u8`) or a content type containing
`mpegurl`, then required the body to start with `#EXTM3U`, so an AES-128 key served as
`application/vnd.apple.mpegurl` (seen live on Pluto `ts_aes` streams, flagged while
surveying 600 catalog channels) was refused with 415 and the encrypted stream died on a
fatal `keyLoadError`. The body now decides: a body that starts with `#EXTM3U` is rewritten
as a playlist; a non-playlist body that a server labels as `mpegurl` at a URL that does not
name a playlist is served as at most 4096 bytes of `application/octet-stream` (same
`nosniff`, `no-store`, the SSRF checks and the 2 MiB read bound untouched); anything larger,
empty, or at a `.m3u8` URL stays a 415, as does a key labelled `text/html`. Witnessed with a
real encrypted stream, not a stub: `ffmpeg` produced a 4-segment AES-128 HLS stream, served
by a local origin through a cloudflared quick tunnel (an ordinary public https host to the
route), and real hls.js in headless Chrome played it through the route: with the key labelled
`mpegurl` it died on `keyLoadError http=415` before and plays after (320x180, key loaded 4
times, 4 fragments parsed, no errors); the same stream with a key labelled
`application/octet-stream` played before and after, and with `text/html` fails before and
after. A key-size table (16 and 4096 bytes served; 4097, 5000, empty, `.m3u8` path, `text/html`
refused) and the 47-row hostile-URL table (0 recorded connections) and the 53 bound checks all
pass on the new code. Not rolled.

## 2026-10-13 — a tenant URL can no longer aim the node at its own network: one outbound guard for webhooks, world-queue targets, SIEM sinks, workflow Redis URLs and remote images

Five features make the NODE send an HTTP request to a URL a tenant sets, and none checked where
it pointed: a webhook endpoint (`WebhookStore`'s plain `reqwest::Client`), a world-queue
`target_url` (`world_queue.rs`, with tenant-chosen headers and body), a SIEM sink
(`enterprise::deliver_siem`, which also sent the sink's bearer token), a project's
`UPSTASH_REDIS_REST_URL` (`world::cmd`/`pipeline`) and, in `fluid-gateway`, the remote image
behind `/_next/image?url=` (allowed only by the tenant's own `images.remotePatterns`, followed
redirects, buffered the whole body before its 16 MiB check, and with `dangerouslyAllowSVG`
returned the fetched bytes verbatim, so a visitor read any internal URL). A tenant could aim any
of them at loopback admin ports, the VPC or the cloud metadata address. The new `hive-outbound`
crate is the one guard: `validate_url` (static: https only, no userinfo, no internal name, no
non-public address literal, including v4-mapped, NAT64 and 6to4 forms), `client()` (connect time:
its resolver drops every address that is not public unicast, so a name that resolved publicly at
registration and rebinds later is refused; no redirects, no proxy) and `following_client()` (the
image fetch: up to 5 redirects, each hop re-validated). Registration refuses a bad URL with 400
(`WebhookStore::add`, `WorldQueue::enqueue` and `EnterpriseStore::set_siem` now return the
refusal, so a caller cannot skip it), and every delivery re-checks, so a row persisted before
this change is refused too instead of delivered. A refusal or failure is recorded as a fixed
sentence (never the URL, address or OS error), and a world REST reply is read at most 32 MiB.
Witnessed on a throwaway auth-enforced node with a connection recorder standing in for the
internal service: before, 11 of 11 attacks reached it (hooks on `http://127.0.0.1` and
`http://[::1]`, on a name resolving to 127.0.0.1, on a public https URL that 307-redirects into
loopback, and a persisted hook; world-queue jobs on both forms; a SIEM sink, which received the
bearer token; a workflow Redis POST; the image optimizer, whose SVG passthrough returned the
internal body to the visitor and which followed a public host's redirect into loopback); after,
the recorder saw 0 connections, the literal-address registrations were 400 and the rest were
refused at delivery ("the host does not resolve to a public address", a 307 recorded and not
followed), while a public https hook, a world-queue job, a SIEM sink, a public SVG and a public
PNG still work (9 of 9). Operator note: nothing to migrate; hooks, queue targets and SIEM sinks on
`http://` or private addresses stop delivering (`GET /v1/webhooks` lists them; re-register on
https), an image `url` must be a public address, and `HIVE_OUTBOUND_ALLOW_PRIVATE=1` lifts the
guard for a developer's machine only. Ships `Cargo.lock` with the new crate. Not rolled.

## 2026-10-13 — ZK roster enrollment runs the same membership check as preview unlock

`POST /api/zk-enroll` (`ui/app/api/zk-enroll/route.ts`) skipped the Clerk membership check
for `personal`, `__personal__` and the empty team and took the team from the request body,
so any signed-in user could enroll their derived key into the shared legacy `personal` ZK
roster, which the node gossips mesh-wide (found by the adversarial review of the preview
unlock commits: minting still needs `/api/preview-unlock`, whose `personal` check requires
an owner email, so nothing unlocked, but the ring a `personal` proof is verified against
was open to arbitrary accounts). The route also answered 403 for a user's own `u_<id>`
namespace (it only knew Clerk orgs) and treated a failed Clerk lookup as "not a member".
The authorization now lives once in `ui/lib/preview-access.ts` (`teamAccess`: org
membership, a user's own `u_<id>`, the legacy `personal` roster only for the owner emails
and only permissive outside production when none is configured; a Clerk failure is
`unavailable`, a retryable 503) and both `/api/zk-enroll` and `/api/preview-unlock` call it;
deleting the route was the alternative, but its only caller `ZkPreviewAuth` (a no-op unless
`NEXT_PUBLIC_ZKAUTH=1`, which nothing sets) would have had to go with it, and the
experiment's background enrollment is what grows the anonymity set. Witnessed by running the
real route handler (Clerk replaced by an identity stub) against a throwaway auth-enforced
node and reading its roster back with a tenant token: before, five non-owner enrollments
into `personal`, `__personal__`, the empty team, `personal` with no owner configured under
production and `personal` with the Clerk lookup throwing all landed in the node roster;
after, all five are 403 or 503 and the roster holds none, the owner, a member of their own
org and a user's own `u_<id>` still enroll, another user's `u_<id>` and a foreign org are
refused, and the preview unlock witness answers exactly as before the refactor. Not
rolled.

## 2026-10-13 — an API key can no longer carry the mesh's `service` role, or a role above its creator's

`POST /v1/apikeys` stored the request's `role` string verbatim (`apikeys::create`) and
`auth::api_key_claims` returned it as the claim role, while the mesh's own delegation role is
the string `service` and three gates trusted that role alone: `GET /v1/browser/metering?
local=true` (answered WITHOUT the operator check), `GET /v1/functions` (every tenant's function
rows: unfiltered under `?local=true`, and the top-level re-scope is skipped for an internal
caller) and `require_fleet_read` (`role == "service" && tenant == "mesh"`). Any tenant, a
`member` included, could mint a key with `role: "service"` and read other tenants' metering and
function stats, and could also mint an `owner` key from a `member` token. The role is now an
allow-list: `apikeys::MINTABLE_ROLES` (`viewer`, `member`, `admin`, `owner`), enforced by
`check_new_role` in `apikey_create` (400 for anything else, 403 for a role above the caller's
own; a platform operator and an unauthenticated dev-mode caller are unbounded), and
`api_key_claims` authenticates a stored key through `apikeys::effective_role`, which maps any
stored role outside the list to `member`, so a `service` key persisted before this change keeps
working as an ordinary member key and never as the mesh. Every mesh-delegation gate now asks one
predicate, `auth::is_mesh_internal` (subject `mesh-internal` AND role `service`; an API key's
subject is `key:<id>`), including the three that read the role alone. Witnessed on a throwaway
auth-enforced node: before, tenant `ta`'s `member` token minted a `service` key (200), that key
read operator-only `/v1/browser/metering?local=true` (200), a `service` key injected into
`state.json` (the pre-fix persisted shape) authenticated as `service` and read it too, and a
`member` minted an `owner` key; after, the mint is 400 `role must be one of viewer, member,
admin, owner`, the persisted key authenticates as `member` and is refused metering (403), and the
`member` to `owner` mint is 403, while owner and member mints of every allowed role, the real
mesh-internal `?local=true` aggregation (200) and an ordinary tenant JWT's 403 on operator
metering are unchanged (14 of 14). The `/v1/functions` half has no local witness: a function row
needs a live function cell and the macOS node has no Firecracker, so it rests on the metering
differential and the shared predicate. Operator note: nothing to migrate; callers that minted
`service` keys now receive 400 and existing ones act as `member`. Not rolled.

## 2026-10-13 — webhooks are scoped to their team: no cross-tenant read, registration or event delivery

`WebhookStore` filtered nothing by owner. `GET /v1/projects/:project/webhooks` (ownership
checked on the project only) returned every team's team-wide `"*"` hooks with the signing
secrets decrypted, so tenant A could read tenant B's hook URL and secret from its own project
and forge signed deliveries; `POST /v1/webhooks` accepted any `projects` entry, or none,
without `project_owned_by`; and `dispatch` selected hooks by project name only, so a
team-wide hook registered by A received every project's events, B's included, and B's
received A's. The store primitive now takes the owner on every operation: `list(Scope,
project)`, `remove(Scope, id)`, `rename_project(team, from, to)`, and `dispatch(store, team,
project, ...)` / `dispatch_durable(..., Audience, ...)` select by (team, project), so a `"*"`
hook means every project of THIS team. The operator surfaces pass `Scope::Operator`
explicitly, and operator-declared incidents use `dispatch_platform`, which still reaches every
team's `"*"` hooks. `webhook_create_team` refuses an anonymous caller and an explicit
`projects` entry the caller's team does not own (403), and the deployment lifecycle outbox
resolves its team from the deployment record. Migration: webhook rows persisted before tenant
ownership have no `team`; they used to normalize to the shared "personal" tenant (listed to
every personal user and fired for every team's events). They now stay on disk, inert (never
listed, never delivered), with one WARN at the first load that counts them: nothing is dropped
and nothing is exposed, and an operator re-creates them under their team. Witnessed on a
throwaway auth-enforced node with a plain HTTP receiver: before, tenant `ta`'s project list
returned `tb`'s team-wide hook URL and secret, `ta`'s team-wide hook received `tb`'s
`database.created` event and `tb`'s received `ta`'s, a hook on `tb`'s project was accepted
(200) and fired, and two injected team-less rows were listed to the `personal` tenant, shown in
`ta`'s project list and fired for `tb`'s event; after, all seven are refused (empty list, no
delivery, 403, the rows inert and still in `state.json`), while each team's own hooks still
list, sign and deliver and a cross-team delete is still 404. Operator note: re-create any
legacy team-less webhooks (the WARN gives the count); the API is unchanged for owners. Not
rolled.

## 2026-10-13 — a queue consumer can only target the queue's own tenant's app, and its batches are signed

A push consumer's `script_name` was accepted unchecked (`queues_api.rs` `consumer_create`) and
the delivery loop POSTed each batch to `cloud.deploy_url(script_name)/__hive_queue_consumer`
with only unauthenticated `x-hive-queue-*` headers: tenant A could create a queue and a
consumer naming tenant B's app, send messages, and B's handler received batches that look
like platform deliveries (and nothing in the request let B tell a real one from a forgery:
the URL is B's public host). Registration now refuses a `script_name` whose host label does
not reach one of the queue tenant's own projects or deployment aliases (403, Cloudflare
error 10006), through `queues::consumer_target_owned`, and the same check runs before every
delivery pass, so a consumer whose target changes owner after registration, or a record that
predates the check, is skipped (one WARN a minute per consumer) instead of delivered. Every
batch now carries `x-hive-signature: sha256=<hmac>` over the exact body (the webhooks
`sign`), keyed by a per-consumer `signing_secret` derived from the fleet-shared at-rest key
(HMAC over `queue_id` and `consumer_id`, length-prefixed, like `zkauth::derive_secret`): no
stored state, no migration, identical on every node and across restarts, and the owner reads
it from the consumer view (`GET /v1/queues/:id`, the consumer create response) to verify
deliveries. Witnessed on a throwaway auth-enforced node with a TLS ClientHello spy standing in
for the apps domain (`HIVE_APPS_DOMAIN=localhost:<port>`, so the SNI host of every connection
the platform opens for a queue is on record): before, tenant `ta`'s consumer on tenant `tb`'s
`victim-app` was accepted (200) and the platform opened 3 connections addressed to
`victim-app.localhost`, and after the operator moved `shared-app` from `ta` to `tb` it kept
opening them (5 connections); after, the registration is 403 with no connection, deliveries
into `shared-app` stop at the move (0 connections), `ta`'s consumer on its own `own-app` still
delivers, two consumers have different secrets and a node restart leaves a secret unchanged.
The signature itself could not be read on the wire locally (the delivery is TLS to the public
URL and reqwest trusts only its bundled roots), so it rests on `webhooks::sign`, the function
webhook deliveries already use. Operator note: nothing to migrate; consumers that pointed at
another tenant's app stop delivering (their messages wait in the queue until retention
expires) and an app that wants to authenticate batches reads its consumer's `signing_secret`.
Not rolled.

## 2026-10-13 — a tenant can no longer snapshot, list or delete another tenant's deployment data through its own project

`POST|GET|DELETE /v1/projects/:project/deployments/:deployment_id/snapshots[/:snapshot_id]`
(`storage_api.rs`) authorized the PROJECT in the path (`require_project`) but acted on the
path's `deployment_id` with no proof that the deployment belongs to that project or tenant:
tenant A could reflink tenant B's data image into a snapshot, list B's snapshots and delete
them by sending B's deployment id (it is in B's URLs) through A's own project. The three
handlers, which the gossip arms that proxy to the hosting node call too, now cross one
`authorize`: `require_project` plus the new `storage_broker::authorize_deployment`, which
requires the deployment (this node's gateway first, then the gossiped peer records) to be a
deployment of the path's project and, when its record carries a tenant tag, of the caller's
tenant (an untagged legacy record inherits the project's owner, which `require_project`
already proved). A foreign deployment answers exactly what an id that does not exist
answers: 404 `unknown deployment` for create and delete, the empty list for list. Witnessed on
a throwaway auth-enforced node (macOS, so no Firecracker backend: a request that passes
authorization answers 501 from the backend check that follows it): before, tenant `ta`'s
cross-tenant create, list and delete against tenant `tb`'s deployment all answered 501, that
is authorized and into the data-image code; after, 404, 200 with an empty list and 404, while
`ta`'s and `tb`'s own deployments still reach the backend check (501) on every route, `tb`'s
project path is still 403 for `ta` and an unknown id is still 404. Operator note: nothing to
migrate and no change for owners; a deployment that no node's view knows is still answered by
the handlers' existing not-found path. Not rolled.

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
36 first segments streamed before, 46 and 35 on one later run and 48 and 37 on the final
run (the differences are origins that flap between runs: `stream8.cinerama.uz` 302s this
IP to a "blocked" slate for a while, curl sees the same), and a 300-channel run played 168
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
