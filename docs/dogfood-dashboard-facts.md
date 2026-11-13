# Self-hosted replicated dashboard/API: current NodeA-bound facts

FACTS ONLY — VERDICT HELD PENDING VERIFICATION

Collected 2026-11-12 by `sc3-research-lead` (SESSION_ID `hive-main-20261112-vitra-sub3`).
This wave investigates the **current NodeA-bound dashboard/backend path only**. No implementation,
no roll, no push, no verdict. The verdict on Narwhal/Tusk usefulness is a separate, later hop AFTER
the implementation wave's verification receipt exists (see
`docs/narwhal-consensus-facts.md` §6 for the exact gate).

## 0. The goal statement, recorded as context (not as a finding)

The user's stated target: `¬SingleNodeBound`, `DashboardAPI ∈ PlatformDeployments`,
`DashboardUI ∈ PlatformDeployments`, `Routed(svc, NearestHealthyNode)`,
`PlatformControlPlane ≡ SelfHostedOn(PlatformDataPlane)` — i.e. dashboard/backend as replicated
platform deployments across P2P nodes, routed to the nearest healthy node, with the control plane
self-hosted on the data plane. The facts below describe what the tree does **now**, not a judgement
about that target.

## 1. Where the dashboard reads state today

All paths absolute under `/Users/dylanwong/fluid/hive`.

| Fact | Live code |
| --- | --- |
| Dashboard server talks to the NODE-LOCAL admin API: `HIVE_ADMIN=http://127.0.0.1:8786` | `ui/.env.example:2`; `ui/README.md:17-18`; `ansible/roles/hive_ui/templates/env.local.j2:10` |
| `/cloud/*` is a Next.js rewrite to the node's admin API ("so there's no CORS setup") | `ui/README.md:17-18`; `ui/proxy.ts:97` (`pathname.startsWith("/cloud/")`); `ui/AGENTS.md` Proxy/CSP section |
| Browser API base is `/cloud`; websocket dials `api.<apex>` directly (rewrites never proxy an Upgrade) | `ui/lib/api.ts:7, 9-23`; `ui/lib/api-catalog.ts:36` (`API_BASE = "https://api.shadw.cloud"`) |
| Route handlers default `HIVE_ADMIN` to the loopback admin port | `ui/app/api/token/route.ts:6`; `ui/app/api/preview-unlock/route.ts:6`; `ui/app/api/zk-enroll/route.ts:5`; `ui/app/api/composio/toolkits/route.ts:9` |
| Node admin binds `127.0.0.1:8786` by default; `8786` is the "node admin / gateway HTTP" port | `crates/hive-cloud/src/main.rs:399`; `crates/hive-cloud/src/raw_ports.rs:219` |
| Pages read `/v1/overview`, `/v1/logs`, `/v1/functions`, `/v1/nodes`, `/v1/waf`, `/v1/bot`, `/v1/cron`, `/v1/workflows`, `/v1/sandbox` | `ui/README.md:61-71` |
| A server prefetch treats `x-hive-anon: 1` as 401, never as data; `fetchOpsServer` has real operator authority and `fetchOpsResult` never renders a refused read as empty | `ui/AGENTS.md` "Backend calls and sessions" |
| Role matrix mirrored row-for-row: `roles::RULES` ↔ `ui/lib/team-role.ts` `TEAM_ACTIONS` | `ui/AGENTS.md`; `AGENTS.md` Security gates |
| `admin_ingress` forwards MUTATIONS to the leader but serves GET/HEAD LOCALLY (plus `/v1/token` and owner-routed paths) | `crates/hive-cloud/src/main.rs:2162-2249`, especially `:2241-2248` |
| No qualifying chain owner = HOLD; forwarded writes answer a retryable 503 | `crates/hive-cloud/src/cluster.rs:185-197`; `crates/hive-cloud/src/state.rs:621-630` |

Consequence, stated in the code's own words (`crates/hive-cloud/src/store_sync.rs:3-11`):
"their GET handlers serve the LOCAL store, so under multi-A DNS a dashboard read lands on a random
node that never saw the leader's writes and returns empty or stale data". Public hosts are
round-robin DNS (`AGENTS.md:64-68`). `docs/leaderless-migration.md:54-60` records why reads were
deliberately moved off the leader: "one wedged/far leader read as 'the platform is down' even
though every node held the data" — the read is node-local by design, which is exactly the dogfood
blocker.

## 2. Which GETs are node-local and therefore answer empty/404 from the wrong node

**Node-local file state.** `persist::data_dir()` returns `$HIVE_DATA` or `~/.hive-cloud`
(`crates/hive-cloud/src/persist.rs:148-153`). Files under it include `state.json`
(`persist.rs:241-247`), `peer_iroh.json` (`persist.rs:155-169`), `audit.jsonl`
(`crates/hive-cloud/src/audit.rs:12`), `thumbnails/` (`crates/hive-cloud/src/admin.rs:17942`),
`acme-account.json` (`docs/leaderless-migration.md:67`), and the managed-DB directories
`$HIVE_DATA/sqlite-dbs/` vs `$HIVE_DATA/browser-dbs/` (`AGENTS.md:180-183`).
`persist::load()` is `serde_json::from_str(&s).unwrap_or_default()` (`persist.rs:242-247`) — a
deserialize error is silent total state loss (`AGENTS.md:53-54`).

**In-process state.** `OnceLock`/`Mutex` singletons that exist only in the serving process:
`crates/hive-cloud/src/leadership.rs:170-173` (`states()` job gate),
`crates/hive-cloud/src/audit.rs:102` (writer registry),
`crates/hive-cloud/src/browser_admission.rs:1709` (signal state),
`crates/hive-cloud/src/admin.rs:475` (mint rate limiter),
`crates/hive-cloud/src/browser_metering.rs:83`,
`crates/hive-cloud/src/browser_db_rest.rs:233`. A GET served by a different node sees none of
them.

**Concrete node-local GETs already named in the tree:**

| GET | Live code | Symptom behind round-robin DNS |
| --- | --- | --- |
| `GET /v1/builds/:id` (`build_get`) | `crates/hive-cloud/src/admin.rs:152, 3010-3033`; route at `:152` | Reads `c.builds` (in-process BuildStore); on a miss tries ONE owner proxy, else 404 (`:3017-3027`) |
| `GET /v1/builds/:id/cancel`, log reads | `admin.rs:3050-3065`; `docs/leaderless-migration.md:357` (P8: `admin.rs:2128,2184,8705,8728,8796`) | Same best-effort leader fallback |
| Region catalog | `admin.rs:752` (`fetch_from_host(&c, &leader, "/v1/regions/catalog", "")`) | Local first, leader proxy as fallback |
| Thumbnails / artifact cache | `admin.rs:17942` (`persist::data_dir().join("thumbnails")`) | File on the serving node only |
| API-key list | PRD row `api-key-create-invisible-node-local-read` (title: "FIXED (commit 443786c): creating an API key at /settings/api-keys returned a token but the key never appeared in the list. apikeys is NODE-L...") | A real witnessed instance of the class |
| Finalized invoice history | PRD row `billing-invoice-history-authority-local` ("BillingStore finalized invoices are never replicated, so invoice history is billing-authority-local") | Open blocker |
| Admin read-route classification | PRD row `admin-read-route-classification` ("classify every admin GET as public, tenant or operator in one router layer ... anycast, serve-ho[st] ...") | Open blocker |
| Browser admission / presence / artifact / sandbox / drive reads | `docs/leaderless-migration.md:357-363` (P9-P13); `browser_admission.rs:2398`, `browser_artifacts.rs:895`, `browser_presence.rs:629`, `drive_api.rs:289`, `sandboxes_api.rs:175` | Each uses `leader_forward_target()` as a bootstrap fallback for an empty local view |
| Audit log | `docs/leaderless-migration.md:822-828` (§7.5: grow-only, "the honest choice is fan-out query ... a partitioned node yields an incomplete answer, which must be surfaced as incomplete, never as empty") | Whole-log replication is not the chosen answer |
| Pub/Sub + Realtime live delivery | `crates/hive-cloud/src/db_replicate.rs:230-236` | Subscribers stay on whichever node they connected to; without fan-out a publish reaches only the leader's local subscribers |

`gm.prd_list({status:"pending"})` on 2026-11-12 returned `pending_total: 617`; the three rows above
are real ids read from that listing (not invented).

**Successor-hop corroboration (2026-11-12, SESSION_ID `hive-main-20261112-vitra-sub3-sub3`,
read-only).** Exhaustive scoped `codesearch` found 173 `.route("/v1/` lines and 145 `get(...)`
registrations in `admin.rs`, 66 `p if` arms in `gossip.rs`, 62 `data_dir()` sites, 223 `OnceLock`
sites, 39 `?local=true` fan-outs and 37 `fetch_from_host` call sites. `roles::RULES`
(`crates/hive-cloud/src/roles.rs:72`, gate `:325`) decides ROLE, not public/tenant/operator, and
unauthenticated GETs never reach it (`auth.rs:514-517, 540-546`); `require_fleet_read` /
`require_auth_read` guards are handler-local, and `/v1/mesh` (`admin.rs:8041-8046`) stays
deliberately anonymous. A later `gm.prd_list` reported `pending_total: 618`; the 617 above is the
earlier read, and one row was added in between. The successor left
`admin-read-route-classification` OPEN with a named blocker (a one-router-layer classification,
7-day report mode, UI-first deploy and a live HTTP re-witness are an implementation wave, not a
read-only facts hop), filed via `prd_note` because `prd_block` is an unknown verb on this runner.
Its B1-B3/B7/B9 cross-check matched this doc. No Narwhal verdict.

## 3. The exact requirements the code states for moving such a GET off node-local

`AGENTS.md:64-68` is the rule; these are its live implementations.

1. **Owner proxy.** `build_get` at `admin.rs:3010-3027`: on a local miss it asks
   `c.leader_forward_target()` and calls `fetch_from_host(&c, &leader, &format!("/v1/builds/{id}"),
   &t)`. `admin.rs:752` shows the same helper for `/v1/regions/catalog`. The owner re-checks the
   bearer and NEVER re-proxies: `gossip.rs:19-20` — "a forwarded call is served LOCALLY with no
   further proxying (`local=true` or a dedicated single-hop arm), so a proxy loop is impossible".
   `admin_ingress` also runs the 2FA step-up re-check on a forwarded mutation
   (`main.rs:2214-2239`) and answers `not_leader_refusal()` (retryable 503) before `serve_local`.
2. **A `gossip::dispatch` arm.** `gossip.rs:11-20`: arms are an ORDERED table, must stay
   **LONGEST-PREFIX-FIRST** (bare `/v1/billing` after `/v1/billing/ledger`, `/detach` before its
   siblings); an **unknown path answers an EMPTY body**, which the sender reads as `NO_HANDLER` —
   that is what makes a new arm safe against a pre-upgrade peer (`gossip.rs:17-18, 744, 1756`).
   The generic store-snapshot arm is `gossip.rs:402-412` matching
   `store_sync::SNAPSHOT_PATH_PREFIX` (`store_sync.rs:30-39`), built by `store_sync.rs:650`.
   `gossip.rs:1002-1018` is the build/log-read arm (checker B's citation). `hive_p2p` has only
   `GOSSIP_GET`/`GOSSIP_POST` (`admin.rs:4410`).
3. **`store_sync::REGISTRY`** (`store_sync.rs:1-24, 39`): one `(name, snapshot, adopt)` triple per
   store; `snapshot` must produce DETERMINISTIC bytes (sorted `BTreeMap`/pre-sorted `Vec`) because
   the follower's change gate is a raw byte comparison; `adopt` declines an empty or unparsable
   payload (`:13-18`). Scope is "only stores whose full contents are safe to replicate wholesale
   **under the single-writer model**" (`:20-24`). `MERGE_STORES` is the per-row merge set
   (`store_sync.rs:530`; `store_follower.rs:130-132`).
4. **`db_replicate::fanout_all`** (`db_replicate.rs:230-259`): fans a write out to EVERY healthy
   peer, no-op when `is_mirror`, with a signed short-lived `x-hive-mirror-tok` / `mesh_team_qs`
   proof verified by `write_scope()` (`:261-280`). `docs/leaderless-migration.md:719-724` records
   the honest limit: "best-effort, no acks consumed, no backfill for nodes that were unhealthy" —
   the same exposure as Tau's single-holder write.
5. **Wholesale-replace authority is NOT optional.** A state row that moves into `REGISTRY` is
   adopted wholesale only from the chain HEAD or a chain-less identity election
   (`cluster.rs:46-55`, `store_follower.rs:157-191`); a backup owner or `Fallback` degrades to
   per-row `MERGE_STORES` only. `Target::still_permitted` re-checks the owner before AND after a
   multi-minute pull (`store_follower.rs:134-148`). `docs/leaderless-migration.md:240-256` says
   plainly "`store_sync` is not a replication mechanism and cannot be reused as one" — 23 of 24
   `adopt` functions are full `load()` replaces.
6. **The request-path funnel is the boundary.** `admin_ingress` (`main.rs:2241-2248`) decides
   local-vs-forwarded per request; `admin_loopback_forward` on `127.0.0.1:8786` must flip in
   lockstep (`docs/leaderless-migration.md:289-292`: the 2026-08-04 witnessed bug was exactly a
   path that bypassed one of them).

## 4. Blockers (facts only; nothing here is implemented)

| # | Blocker | Evidence | Status |
| --- | --- | --- | --- |
| B1 | Dashboard server is pinned to the node it runs on via loopback `HIVE_ADMIN` / `/cloud` rewrite; `DashboardUI`/`DashboardAPI` are not `PlatformDeployments` | `ui/.env.example:2`, `ui/README.md:17-18`, `ui/proxy.ts:97`, `main.rs:399` | VERIFIED |
| B2 | Public hosts are round-robin DNS; `admin_ingress` serves GET/HEAD LOCALLY, so node-local files / `OnceLock` / `Mutex` answer empty/404 from the wrong node | `AGENTS.md:64-68`, `main.rs:2242-2243`, `store_sync.rs:3-11`, `persist.rs:148-153, 242-247`, `audit.rs:102`, `admin.rs:475, 1709, 17942` | VERIFIED |
| B3 | Owner proxies exist only for selected paths (builds, region catalog, browser/sandbox artifacts); a new GET needs its own proxy + longest-prefix-first `gossip::dispatch` arm, and an old peer answers unknown paths with an EMPTY body | `admin.rs:3010-3027, 752`, `gossip.rs:11-20, 402-412, 744, 1756`, `docs/leaderless-migration.md:357-363` | VERIFIED |
| B4 | `store_sync::REGISTRY` is wholesale-replace under the single-writer model; two writers would "mutual clobbering on a 60s tick" | `store_sync.rs:1-24`, `store_follower.rs:157-191`, `docs/leaderless-migration.md:240-256` | VERIFIED |
| B5 | `db_replicate::fanout_all` is best-effort with no acks consumed and no backfill for unhealthy nodes | `db_replicate.rs:230-259`, `docs/leaderless-migration.md:719-724` | VERIFIED |
| B6 | Leader-local state is lost on failover: ACME account key, DNS reconciler damping, `BillingStore.meters`, `git_poll_seen`; the `-$55` counter-reset incident is documented | `docs/leaderless-migration.md:61-75` | VERIFIED (design doc, explicitly says the tree wins over the doc) |
| B7 | Finalized invoice history is billing-authority-local, never replicated; API-key list had the same class of bug (FIXED 443786c); admin GET classification is still open | `gm.prd_list` rows `billing-invoice-history-authority-local`, `api-key-create-invisible-node-local-read`, `admin-read-route-classification` | VERIFIED (rows are real, read from the live PRD store) |
| B8 | `persist::load()` silently defaults on a deserialize error; a serde-REQUIRED key removed from a persisted struct in one release wipes state | `persist.rs:242-247`, `AGENTS.md:53-54` | VERIFIED |
| B9 | A `GET` over the merged fleet view must re-scope peer rows (`?local=true` answers UNFILTERED) | `AGENTS.md:150-152` (`admin::non_operator_node_view`) | VERIFIED |
| B10 | Promotion/ownership change starts local writes at `is_control_plane_leader()` flip with no coordination with the promotion fetch | `store_sync.rs:749-771`, `state.rs:641-649` | CONDITIONAL (documented in code, not witnessed on the fleet) |
| B11 | `Routed(svc, NearestHealthyNode)` for the dashboard was not found in the code read this wave; Seer answers the deploy zone affinity-first from `apps_host_owner`, and `db_gateway` bounds only unproven/draining connections | `AGENTS.md` DNS and managed-data sections; `ui/lib/api.ts:9-23` (websocket dials `api.<apex>` directly) | CONDITIONAL — absence in the files read, not an exhaustive absence proof |

## 5. Production-change boundary (hard stop)

A roll/deploy is a **production change**: the operator's word FIRST, then `ansible/AGENTS.md`
(dashboard: `ui/AGENTS.md`). Per `AGENTS.md:38-42`: prove the RUNNING binary with
`strings -a /proc/<MainPID>/exe | grep -c '<log text>'`, **never** by grepping a Rust format
literal; never `git_reword` during a roll (it rewrites every later SHA). Additional live-fleet
constraints from the same section: `fc-orlando` SHARES its host with `hive-devhub.service` (stop by
MainPID, never `pkill -x hive-cloud`); SSH is `ssh -i ~/.ssh/id_ed25519 root@<ip>` with
`sysadmin@` + `sudo -n` on Orlando; the admin API is plain HTTP loopback `:8786`; a GET over
node-local data behind round-robin DNS answers empty/404 from the wrong node; verify ports from a
FLEET vantage, never the laptop. The dashboard ships SEPARATELY from the backend
(`ui/AGENTS.md` "Build, deploy, witness"; `scripts/deploy-ui-fleet.sh`), and a UI-only commit does
not change the backend binary. **This wave performed zero rolls, zero pushes, zero code edits.**
Any change to `admin_ingress`'s local-vs-forwarded branch, `gossip::dispatch`, `store_sync`, or the
`HIVE_ADMIN`/`/cloud` rewrite is a production change requiring the operator's word.

## 6. Verdict HELD (exact gate)

No verdict or recommendation is issued. Gate (user's words, verbatim): "Once you have finished and
verified completion of the task above proceed with researching and analyzing if narwhal consensus
is useful for the shadow decentralized network as a whole taking account for everything including
our billing ledger, control plane etc and where it fits. We never want a split brain or something
of that nature". The verdict hop is separate and comes AFTER the implementation wave's verification
receipt exists. This doc is the FACTS wave for dogfood only.

**Gate refinement (2026-11-12; receipts below reported by the orchestrator, NOT independently
re-run in this facts wave).** Partial verification receipts now exist: `cargo build -p
hive-contract --offline --bin contract-billing` exit 0 (4.63 s); `./target/debug/contract-billing`
exit 0 with `RESULT PASS probes=11 replay_ok=true signed=9 proofs=7 rewritten_detected=true
omitted_detected=true chain_tamper_detected=true` (G1/G2/G3 deserialize bypass refused);
`cargo check -p hive-cloud --offline --bin hive-cloud` exit 0 (`Finished dev profile in 1.68s`),
with one earlier transient E0308 mid-edit and both runs logged. These are PARTIAL and do not
satisfy the gate. The remaining condition is the **FOUR-LAYER gate on a FINAL FROZEN tree**:
`cargo test --workspace --locked`, `scripts/cli-acceptance.sh`,
`check-contract-bindings.py --check`, and `git diff --check` (Linux AND macOS per `AGENTS.md`).
PRD row `narwhal-usefulness-verdict-held-sc3` stays OPEN with that named blocker; a dedicated
verdict hop is dispatched only after that receipt exists. No recommendation is issued here, and the
production-change boundary in §5 is unchanged (a roll still requires the operator's word first).

## 7. Executed witnesses for this hop

- `gm.codesearch literal` (own live run, cwd `/Users/dylanwong/fluid/hive`):
  `1794542827183-6996-f4dbc98b0f3fe3df` (`127.0.0.1:8786` in `ui`, 18 hits),
  `1794542837245-7006-91539ecf0f1fca47` (`HIVE_ADMIN` in `ui`, 14 hits),
  `1794542849640-7017-edb5a0ec48258c72` (`ADMIN_URL`, 0 hits, exhaustive),
  `1794542968868-7036-1b1355e55bb8f77b` (`api.` in `ui/lib`, exhaustive),
  `1794542981581-7055-70f6a80b3a5b2fba` (`leader_forward_target`, 28 hits),
  `1794543042859-7090-a88612c986d1cdf4` (`fn data_dir`, exhaustive),
  `1794543051554-7101-9726622ab1fb80a1` (`pub fn load` in `persist.rs`, exhaustive),
  `1794542705054-6860-b24e92823a5b21cb` (`SNAPSHOT_PATH_PREFIX`, exhaustive),
  `1794542733684-6886-140e8990138aaa27` (`GOSSIP_GET`),
  `1794542744460-6900-afbf2cb2d892ad02` ("unknown path", exhaustive),
  `1794542759205-6920-ef45f78da83b4a7` (`audit.jsonl`, exhaustive),
  `1794542775221-6930-5443262ada6824ab` (`thumbnails`, exhaustive),
  `1794543757329-7630-1eb1e7bd2e7dfd6a` (`cloud/` in `ui/proxy.ts`, exhaustive),
  `1794542314029-6402-7e895131eca4e1e0` (`build_get`, 16 hits),
  `1794542325665-6421-2575ad164d9aa9ac` (`fetch_from_host`, 104 hits),
  `1794542304907-6387-96209165b74c0e9` (`fanout_all`, 6 hits),
  `1794542530780-6667-b6dd1ef930cf5a7d` (`data_dir()`, 82 hits),
  `1794542543340-6681-7bb693b47f649b0` (`state.json`, 52 hits).
- `gm.prd_list({status:"pending", limit:1000})` in my own live run (2026-11-12):
  `pending_total: 617`, real ids `api-key-create-invisible-node-local-read`,
  `billing-invoice-history-authority-local`, `admin-read-route-classification`.
- Host `Read` of located paths: `main.rs:2160-2249, 2408-2442`; `admin.rs:3005-3074`;
  `gossip.rs:1-30, 398-427`; `store_sync.rs:1-40, 748-777`; `store_follower.rs:1-205`;
  `persist.rs:140-169, 240-267`; `health.rs:1-160`; `db_replicate.rs:230-274`;
  `billing.rs:315-354, 505-545`; `state.rs:490-529, 590-659`; `region.rs:825-852`;
  `ui/lib/api.ts:1-45`; `ui/README.md:55-74`; `docs/leaderless-migration.md:1-220, 220-460,
  660-839`; `docs/verifiable-database.md:170-187`; `crates/hive-contract/src/lib.rs:1-16`;
  `crates/hive-contract/src/replicate.rs:1-60`.
- Checker B (read-only, 2 valid refutations / 2 votes / 0 errors) independently located
  `gossip.rs:1002-1018`, `admin.rs:7126-7160, 12045-12066, 14243-14255, 16745-16767`,
  `store_sync.rs:530, 781`, `store_follower.rs:137-148, 163-214`; final phase-status dispatch
  `1794543790973-7649-7e620e22cdeff4d1`.
