# Leaderless control plane — design and staged migration

Status: **design only.** No code in this document has been written. It is the
plan a later implementation pass executes, stage by stage, and every stage is
independently shippable and independently reversible.

Scope: removing `cluster::Cluster::control_plane_owner` — "the leader" — from
the 66 call sites that consult it, replacing each with either a leaderless
convergent mechanism or a **deterministic per-key owner** derived from
gossiped state with no election. It does *not* propose adding a consensus log
(Raft/openraft/etcd) to this platform; §6 explains why, with the taubyte
comparison the request asked for.

Everything below cites `file:line` read from the tree at
`8fbb60e` (2026-08-05). Where a study's claim disagreed with the tree, the
tree wins and the disagreement is called out.

---

## 1. The premise, checked

> "the leader model stores the entire network's data on 1 node instead of auto
> balancing/distributing it across every node"

**This is wrong about storage and roughly half-right about writes. Do not
build the project on the storage claim, because the migration does not fix it
— it makes it slightly worse.**

### 1.1 Every node already stores 100% of control-plane state

Four independent mechanisms put the whole fleet's state on every node, and
none of them is leader-gated on the *storing* side:

| Mechanism | Where | Leader-gated? |
|---|---|---|
| `PlatformSnapshot` capture + fsync to `$HIVE_DATA/state.json` | `persist.rs:344-388` (`capture`), `persist.rs:210-230` (`save`) | **No.** `persist::restore` + `persist::spawn_persister` run unconditionally at boot (`main.rs:674-677`) |
| Per-tenant namespace docs `$HIVE_DATA/ns/<ns>.json` | `persist.rs:247-316` (`namespaced`), `save_namespaces` | No |
| GuardianDB (`iroh-docs` 0.101, Willow range reconciliation) | `guardian.rs:52` opens the single `hive-state` namespace; `spawn_guardian_snapshot_loop` `main.rs:2520-2532` — *every* node captures and replicates its own snapshot every 120s | No |
| Relational mirror, 11 tables | `relational.rs:1-7` — "**Every node** opens the same named database ('hive')" | Partly (see §4.B) |

`store_sync::REGISTRY`'s 24 stores (`store_sync.rs:95-484`) are *also* pulled
onto every follower every 60s (`main.rs:3617-3673`). The number of nodes
holding a given control-plane record today is **N**, not 1.

Tenant *workload* data is likewise already distributed: `schedule.rs:155`
`place()` applies a **hard** free-disk filter (`schedule.rs:118-131`,
`HIVE_PLACEMENT_DISK_FLOOR_GB`), and `git.rs:1116-1147` explicitly refuses to
build or host locally when placement chose remote targets.

### 1.2 What is actually true

The leader is the single **writer**, not the single **holder**:

- Every mutation on the public `api.`/`admin.`/`webhook.` hosts and on the
  dashboard's loopback admin path serializes through one node
  (`main.rs:1758-1772`, `main.rs:1490-1506`). That is a real latency and
  blast-radius cost. The codebase already conceded half of it: reads were
  deliberately moved off the leader, with the reason written in-line at
  `main.rs:1745-1754` — "one wedged/far leader read as 'the platform is down'
  even though every node held the data."
- A small, specific set of state genuinely *is* leader-local and
  **unreplicated**, and losing it on failover is a live defect today,
  independent of this project:

  | Leader-local state | Where | What a failover loses |
  |---|---|---|
  | ACME account key | `acme.rs:306-313` — "`$HIVE_DATA/acme-account.json` (leader-local; a new leader creates its own)" | The LE account identity; a new leader silently starts a *different* account against the same shared rate-limit budget |
  | DNS reconciler cross-pass memory | `vercel_dns.rs:1821-1836` — `PublishDamping`, `DelegationDamping`, `ReconcileGuards`, `api_ns_published`, `backoff` are plain `let mut` bindings inside the spawned task | Every damping counter resets to 0, so `HEALTHY_PASSES_BEFORE_REPUBLISH`/`UNHEALTHY_PASSES_BEFORE_WITHDRAW` (`vercel_dns.rs:45,54`) provide **zero** damping across a handover — exactly when flapping is most likely |
  | Billing meter watermarks | `billing.rs:405` `meters: RwLock<HashMap<String, MeterState>>` — **not** in `BillingStore::snapshot()` (`billing.rs:950-955`), not in `PlatformSnapshot`, not in the `store_sync` billing entry | The per-tenant `last` cursor; the successor re-baselines from `MeterState::default()` |
  | Git-poll dedup | `state.rs:247` `git_poll_seen` | The SHA dedup set that keeps a push from deploying twice |

  Note the meter case is not merely "resets" — `meter_usage`'s own comment
  (`billing.rs:540-560`) records the witnessed incident where counter resets
  re-billed the whole fleet and drove a tenant to −$55 against a $5 allowance,
  tripping the `can_deploy` credit lock.

### 1.3 The correction that matters

If the goal is "no single node holds everything," **leaderless CRDTs move in
the wrong direction**: per-record revisions, tombstones, and per-column
Lamport versions make every node hold *more* bytes than it does now. The lever
for that goal is **sharding**, the boundary already exists and is unused —
`persist::namespaced()` (`persist.rs:247-316`) and guardian's
`ns/<namespace>/state` keys already partition state per tenant. Letting a node
hold a *subset* of namespaces is a separate, orthogonal project. It is not
this one, and this one does not deliver it.

The defensible reasons to do *this* project are:

1. Mutation latency: every write from Europe/Asia pays an RTT to whichever
   node heads `HIVE_CP_OWNER_CHAIN` (live: `fc-sanjose,fc-bangkok,fc-virginia`,
   `ansible/inventory/hosts.ini:95`).
2. Blast radius: a wedged leader stops *all* writes fleet-wide, including
   deploys.
3. The leader-local state in §1.2 is lost on every handover, and fixing it is
   required by this migration anyway (Stage 1) — so the first stage is worth
   shipping even if the rest is abandoned.

**Gate for continuing past Stage 4** (see §5): measure p50/p99 mutation
latency bucketed by origin region vs. leader region, and leader-unavailability
minutes/month. If both are small, Stages 5–9 buy correctness cleanliness at a
high price and should be deferred.

---

## 2. What "leaderless" has to mean here

Three distinct problems hide under one word. Each needs a different answer,
and conflating them is how this kind of migration goes wrong.

**Class C — Convergent.** Concurrent writes from any node can be merged into
one deterministic answer. Requires a *merge function*, not coordination.
Everything in `store_sync` should be here and only one entry is
(`browser_presence`, §3.3).

**Class K — Key-owned single-flight.** Exactly one node must *run* something
at a time, but which node is irrelevant and derivable. Answer: a deterministic
per-key owner computed from replicated state, plus a fenced lease. No
election, no vote, no log. Already proven in this codebase three times (§3.1).

**Class X — Externally serialized.** An external system imposes the
constraint: Vercel's zone record set, Let's Encrypt's rate-limit budget. The
external API will never honour our fencing token, so Class K is necessary and
not sufficient — the operation must *also* be planned as an idempotent diff
and re-checked immediately before each destructive step.

**Class I — Idempotent-instead-of-exclusive.** The strongest answer: make
double execution converge to one outcome so exclusivity is unnecessary.
Whenever this is reachable, it beats Class K, because it survives roster
disagreement, partitions, and rollout skew for free. The billing ledger is
reachable (§4.B1) and should be moved here rather than coordinated.

**Class O — Owner-proxy read.** The leader is used only as "someone who
definitely has the bytes." Replace with per-key owner resolution plus a
bounded fan-out; never with a global leader.

**Class D — Delete.** The responsibility evaporates once the underlying store
converges.

---

## 3. The primitives already in this tree

The design deliberately adds almost no new mechanism. Three things already
work in production and are the entire foundation.

### 3.1 Rendezvous (HRW) ownership — leaderless by construction, three live users

`lease::hrw_owner` (`lease.rs:148-168`): deterministic FNV-1a over
`"{key}\0{node}"`, `max_by_key`. Deliberately FNV and not `DefaultHasher`
"(stable across std versions)… so all nodes compute identical rendezvous
weights."

Three live users, each with a different eligibility filter over the same
primitive:

| User | Key | Candidate set | Where |
|---|---|---|---|
| Container placement | project/subdomain | live nodes that gossiped they hold the container, region-constrained | `main.rs:2452-2489` |
| Inference coordinator | project | healthy GPU nodes in the largest-total-VRAM region | `inference.rs:97-116` |
| Per-project inference port | project | — (pure hash, `50100 + fnv%900`) | `inference.rs:70-72` |

The inference doc comment (`inference.rs:87-96`) is the argument for HRW over
modulo, written from a live incident: with modulo, "a rolling GPU-group
restart shifted every project's slot and the reconcile killed a
25-minutes-into-load 14B endpoint mid tensor-upload." **HRW's minimal-churn
property is the load-bearing one** — a roster flap moves only the keys that
hashed onto the departed node, and when the roster recovers every key returns
to the same owner. That is precisely the property a control plane under
per-observer health verdicts needs.

**But HRW alone is agreement conditional on agreement about the input set.**
Two nodes with different rosters compute different owners, and AGENTS.md is
explicit that this happens routinely: "A health verdict is per-OBSERVER, so
peers legitimately disagree." HRW is a *policy*, never a *safety* mechanism.

### 3.2 The fenced lease — the safety half

`lease.rs` (module doc `:1-13`): `ContainerLease{key, owner, epoch, region,
acquired_ms, expires_ms}`; `merge` (`:74-85`) is a proper CRDT join — higher
epoch wins, epoch tie broken by later expiry — and it converges over gossip
(`main.rs:2922-2930` pulls `/v1/leases` from each peer per anti-entropy round
and `merge`s every entry; served at `gossip.rs:378`).

`acquire_or_renew` (`lease.rs:88-125`): own it → renew; someone else holds a
*live* lease → refuse; free/expired → take with `epoch + 1`.

**Honest limitation that the design must not paper over:** `acquire_or_renew`
consults only the *local* map. Two nodes that each believe they are the HRW
preference will both take the lease locally, each with `epoch+1` from their
own view, and the conflict resolves only after the next gossip round. So the
lease as it stands gives **eventual single ownership plus a fencing token**,
not immediate mutual exclusion. For containers that is correct and cheap (two
containers briefly, then one releases). For Let's Encrypt orders and Vercel
zone deletes it is not sufficient on its own.

### 3.3 A real merge, already written, with the postmortem attached

`browser_presence::adopt` (`browser_presence.rs:216-291`) is the template
every convergent store must copy. Its doc comment *is* the argument:

> "This used to be `*state = incoming` behind an `incoming.version <=
> state.version` gate, and that silently DESTROYED presence records. Every
> node's `version` is wall-clock anchored… whichever replicated with the
> higher version overwrote the other node's entire map, and the browser
> admitted through the losing node vanished from the constellation with no
> error logged anywhere."

Rules (an LWW-element-set / add-wins map with tombstone dominance): union both
sides; record wins on higher `revision`; a tombstone at or above a record's
revision beats that record; tombstones union by max revision; resurrect
without tombstone is allowed and bounded by a 90s TTL.

**`browser_admissions` still carries the exact bug `browser_presence` was
fixed for** (`browser_admission.rs:552-576`: version-gated wholesale replace,
`next_version()` wall-clock anchored at `:289-292`). It is safe today *only*
because there is one writer. This must be fixed first in Stage 5 regardless of
whether the rest of the migration proceeds.

### 3.4 The three replication substrates, and their actual guarantees

"Shadow protocol" is not one thing. Picking the wrong substrate per
responsibility is the main technical risk in this design.

| Substrate | Conflict rule | Clock-dependent? | Granularity | Use for |
|---|---|---|---|---|
| `gossip::dispatch` over `hive-p2p` QUIC | none — HTTP-shaped unicast request/response, at-most-once, no dedup, no ordering; replay window `HIVE_GOSSIP_TS_WINDOW_SECS` 300s | n/a | per call | RPC, pulls, fan-out. **Never** as a replication log |
| GuardianDB (`iroh-docs` 0.101) | wall-clock LWW per key, strict `>`, tie → lowest author id | **Yes, absolutely** | whole value (SQL layer: whole *row*) | durable blobs, cross-node docs, snapshot exchange |
| `hive-crsql` (cr-sqlite v0.17, `crates/hive-crsql`) | causal length → per-column Lamport `col_version` → value compare → `site_id`; **the wall-clock `ts` column is stored and never consulted** | **No** | per (row, column) | anything relational that must converge under concurrent writers |

`hive-crsql` is already a workspace member (`Cargo.toml:25,89`), already a
`hive-cloud` dependency (`crates/hive-cloud/Cargo.toml:24`), already carrying
a bounded, gap-detecting, watermark-chained wire protocol
(`hive-crsql/src/lib.rs:16-62`: per-origin-site watermarks, `SyncGap` refusal,
one transaction per batch, canonical binary encode), and already in production
use for `browser_db` (`browser_db.rs`, 932 lines). It is the correct substrate
for durable multi-writer relational state, and the one that does not make NTP
a correctness dependency.

### 3.5 The one thing that must go: `store_sync`'s wholesale replace

`store_sync.rs:20-36` states its own scope: the follower's change gate is a
"raw byte comparison," `adopt` is a full `load()`, and the registry covers
"only stores whose full contents are safe to replicate wholesale **under the
single-writer model**." `main.rs:3627-3629` repeats it: "Wholesale replace
(not merge) is correct under the single-writer model: the leader IS the
authority."

23 of the 24 `adopt` functions are `c.<store>.load(...)` = full replace. Under
two writers this is mutual clobbering on a 60s tick, permanently oscillating,
losing data on every flip. The `if m.is_empty() { return None }` guards protect
against a *booting* leader; they do nothing against a populated-but-different
peer.

**`store_sync` is not a replication mechanism and cannot be reused as one.**
Stage 5 is 23 separate merge-semantics decisions. There is no shortcut.

---

## 4. Complete responsibility map

Every leader consultation in the tree, with its replacement. Counts verified:
66 direct `is_control_plane_leader()` / `control_plane_leader()` call sites —
`admin.rs` 25, `main.rs` 12, `browser_admission.rs` 7, `raw_ports.rs` 5,
`state.rs` 3, `push.rs` 3, `browser_presence.rs` 3, `sandboxes_api.rs` 2,
`gossip.rs` 2, `browser_artifacts.rs` 2, `inference.rs` 1, `git.rs` 1 — plus
three indirect `Cluster::control_plane_owner()` callers (`vercel_dns.rs:1862`,
`acme.rs:566`, `state.rs:470`).

### M. The mechanism itself

| # | Thing | Where | Fate |
|---|---|---|---|
| M1 | `Cluster{me, epoch, owner}`, `observe_owner` bumps epoch on ownership change | `cluster.rs:51-99` | **Keep the epoch.** It becomes a global fencing generation for `ownership.rs`. Drop `owner`. |
| M2 | `HIVE_CP_OWNER_CHAIN` parse | `cluster.rs:103-112` | Demoted to an *ordering hint* for the eligible roster, then deleted at Stage 9 |
| M3 | `control_plane_owner` — the single resolution point | `cluster.rs:119-152` | Replaced by `ownership::owner_for(key, class)`. Its **eligibility predicate is kept verbatim** (`healthy && peer_id.is_some() && (public_ip \|\| public_ip6)`) — it encodes the NAT'd-dev-laptop protection documented at `cluster.rs:157-166` |
| M4 | `billing_leader` / `billing_leader_with_pref` — lowest healthy `peer_id`, public preferred | `cluster.rs:167-206` | Deleted. Lowest-peer_id is a global election; HRW is per-key and churn-stable |
| M5 | `elect_among` — per-candidate-set election, used by `world_queue::is_primary_for_team` (`world_queue.rs:227-250`) | `cluster.rs:213-228` | **Keep, but re-point at HRW.** `world_queue` already scopes candidates to nodes hosting the tenant — exactly right; only the tiebreak (lowest peer_id) should become `hrw_owner(team, candidates)` so a roster flap does not reassign every tenant's primary at once |
| M6 | `CloudState::control_plane_leader()` — recomputed every call, no cache | `state.rs:467-482` | Deleted at Stage 9 |
| M7 | `is_control_plane_leader()` + isolation gate (`mesh_health().isolated`, `state.rs:722`) | `state.rs:498-503` | **The isolation gate is kept and generalized:** `ownership::try_own` returns `None` when isolated. A node that sees zero expected peers must never own anything |
| M8 | `spawn_cluster_loop` 3s re-resolve, gossips `cp_epoch` | `main.rs:2496-2508`, `hive-edge/src/region.rs:112,391` | Becomes the ownership-roster damping tick |
| M9 | Epoch fencing on forwarded mutations (`x-hive-cp-epoch`) | `main.rs:1685-1707`, `:1936` | Generalized: the fence token becomes `(key, epoch)` and rides Class K/X work, not just forwarded HTTP |
| M10 | `/v1/cluster`, `/v1/status` expose `ClusterStatus{term, leader, is_leader, members, consensus}` | `cluster.rs:38-49`, `admin.rs:43,4886-4894,9484` | **Wire shape preserved** (dashboard reads it). `leader` becomes `""`, `consensus` becomes `"ownership"`, `term` keeps carrying the epoch |

### R. Request routing — the funnel that makes everything else true

| # | Responsibility | Where | Class | Replacement |
|---|---|---|---|---|
| R1 | `admin_ingress`: mutations → leader, GET/HEAD → local | `main.rs:1758-1772` | funnel | Becomes a **per-route table**, not one global branch: a route whose store has a merge (Stage 5) serves mutations locally. This is the no-flag-day mechanism — routes migrate one at a time |
| R2 | `admin_loopback_forward`: same rule on `127.0.0.1:8786` (dashboard `/cloud` proxy) | `main.rs:1490-1506` | funnel | Same table, same flip. Must migrate in lockstep with R1 — the witnessed 2026-08-04 bug was exactly a path that bypassed one of them |
| R3 | `admin_forward_to_leader` + `leader_forward_candidates` + `leader_client`; SNI-pinned to registry IPs, **fails closed**, never plain DNS | `main.rs:1813,1856,1897-1995` | keep, retarget | Becomes `forward_to_owner(key)`. The fail-closed and SNI-pinning properties are load-bearing and carry over unchanged |
| R4 | `not_leader_refusal` / `is_not_leader_refusal`; emitted **before** `serve_local` so "provably not applied" ⇒ safe to retry | `main.rs:1783-1801` | keep | Becomes `not_owner_refusal`. The pre-application ordering is the whole reason retry is safe; preserve it exactly |
| R5 | Exemptions: `/v1/token` (pure HS256, writes nothing), `x-hive-internal`, `x-hive-admin-forwarded` | `main.rs:1497-1500,1671-1673,1760` | D | Unchanged; they stop mattering once R1's table is empty |
| R6 | `forward_mutation_to_leader` — a *second* in-handler forwarder over `node_admins` HTTP | `admin.rs:9549-9613` | keep, retarget | Same retarget as R3. Two forwarders is already a smell; Stage 9 collapses them |

### E. External-API single-flight — Class X, the hardest rows

| # | Responsibility | Where | Why it cannot be Class C | Replacement |
|---|---|---|---|---|
| **E1** | **Vercel DNS reconciler** — publishes healthy IPs to `api.{platform}`, `*.{apps}` + apex, `relay.`, `discovery.`, per-DB A records, affinity records, geo NS delegation | `vercel_dns.rs:1804` `spawn_reconciler`, gate `:1858-1869` | External API with a rate limit *and* a globally shared record set. `CREATE_PACING = 1100ms` (`:990`) is a **single-writer** budget; N reconcilers = N× create rate. Two divergent healthy-set views produce a create/delete treadmill on the *same* records (witnessed 2026-07-29: 6–15 writes/30s pass, sustained 429s, real address records briefly DELETED). Damping is per-process, so two writers halve effective K | **Class K on key = zone name** (`shadw.app`, `shadw.cloud`), Exclusive class ⇒ quorum-confirmed fence. Plus (Stage 1) `PublishDamping`/`DelegationDamping`/`ReconcileGuards`/`api_ns_published`/`backoff` move into replicated state so a handover does not reset damping — a defect **today**, not only under leaderless. Plus a fence re-check immediately before every phase-0 delete |
| **E2** | **ACME issuance (DNS-01)** | `acme.rs:534` `spawn_acme`, gate `:566-571` | LE 5-duplicate-certs/168h per *exact identifier set*; per-node accounts do not help (they make N distinct orders against one shared budget) | **Class K on key = the sorted identifier set** — literally LE's own rate-limit key, so the ownership key and the constraint are the same object. Plus the account key moves out of `$HIVE_DATA/acme-account.json` (`acme.rs:306-313`) into a `HIVE_SECRET_KEY`-sealed replicated store so every node acts as **one** account |
| **E3** | ACME orphan-TXT sweeper (inside E1's pass), deletes `_acme-challenge.*` unknown to the *local* in-flight store past `ACME_ORPHAN_MIN_AGE_MS` (`vercel_dns.rs:60`, 15 min) | `vercel_dns.rs:1218-1230` | Under N writers each sweeper sees the other N−1's live challenges as unknown; only the age gate stands between this and mutual sabotage, and a retrying order exceeds it | Depends on E5-style convergence of `acme_challenges` (S3 below): the sweeper must consult the **merged fleet-wide** challenge set, never the local one. Once it does, the sweeper itself is safe to run anywhere and drops out of Class X |
| E4 | Incident auto-open from the reconciler (dark name, delegation hold, LE rate-limit, restore failure) | `vercel_dns.rs:1301,1336,1732,1773,2038,2113`; `acme.rs:625-634` | `incidents::open` does not dedup, so N nodes × 1 per pass per condition buries the real incident | **Class I.** Key each auto-opened incident by a stable id derived from `(kind, subject)`. N openers then converge to one row and exclusivity is unnecessary |
| E5 | Operator incident CRUD | `admin.rs:9617,9645,9683` via `forward_mutation_to_leader` | Store is wholesale-replicated | **Class C** — per-incident LWW with tombstone (Stage 5) |

### B. Money

| # | Responsibility | Where | Class | Replacement |
|---|---|---|---|---|
| **B1** | **Billing meter loop** — fleet usage → delta charge → ledger → invoice | `main.rs:3730` `spawn_billing_meter_loop`, election `:3753-3763`, 2-tick stability window `:3771` | **I**, with K for efficiency | Three changes, in order: (a) `LedgerEntry.id` stops being `format!("led_{}", Uuid::new_v4()…)` (`billing.rs:651-653`) and becomes **deterministic** — `led_{blake3(tenant \| window_start_ms \| canonical(counter_snapshot))}[..12]` — so two writers converge to ONE row instead of two; (b) `acc.balance_cents` / `acc.used_cents` stop being stored mutable counters mutated read-modify-write (`billing.rs:515-535`) and become **derived** by folding the ledger, which is already "the financial record and must outlive the account" per AGENTS.md; (c) `meters` (`billing.rs:405`) becomes replicated and **max-merged per counter** — the counters are monotonic, so max is the correct join, and `meter_usage`'s existing decrease-means-reset handling (`billing.rs:544-560`) already errs in the right direction. After (a)–(c), double metering is *convergent*, not double-charging, and per-tenant HRW ownership becomes an efficiency choice rather than a correctness one |
| B2 | `billing_authority_node` + `proxy_billing_read` | `admin.rs:10630-10642` | D | Disappears once B1 makes every node's billing view converge |
| **B3** | **Plan/tier writes** — `apply_plan_everywhere` writes `c.teams` **and** `c.billing` non-atomically | `admin.rs:7549-7556` | **C, by schema change** | Do not coordinate the two halves — **collapse them.** Tier becomes ONE replicated versioned record; `team_plan()` and the billing quota path both *read* it. The both-halves-or-neither invariant then holds by construction instead of by discipline. Note this drift is documented as already happening under a single writer (`teams.plan` unanimously `enterprise` while `billing.plan` read `hobby` on 7 of 8 nodes) — the single writer was never actually preventing it |
| B4 | `apply_admin_enterprise` inside `mint_token` — leader-gated inside an endpoint exempt from leader-forwarding (R5) | `admin.rs:535-543` | I | Already idempotent by construction (writes only when the floor differs). Drop the gate once B3 lands |
| **B5** | Relational mirror — billing section, one `BEGIN;…COMMIT;` per tenant | `main.rs:3667-3708`, `relational.rs:1-35` | K → C | While the source of truth is single-written, this is fine. Once B1 lands, the mirror's own single-writer-by-construction argument (`relational.rs:19-25`) is gone and it must move onto `hive-crsql` CRR tables (per-column Lamport, clock-independent) rather than GuardianDB SQL, whose `Consistency::LocalFirst` gives whole-**row** wall-clock LWW (`vendor/guardian-db/src/sql/guardian_storage.rs:40-53`; its `Strict` mode is explicitly not implemented) |
| B6 | Relational mirror — teams/members | `main.rs:3609-3616` | C | Follows B3 |
| B7 | Relational mirror — deployments | `main.rs:3600-3607` | D | **Already leaderless**: each node syncs only its own `gw.list()` rows, single writer per row by construction. This is the model the other two sections should copy |

### S. `store_sync` — the 24-store leader→follower pump

| # | Responsibility | Where | Class | Replacement |
|---|---|---|---|---|
| S1 | Follower adoption loop: pull each store's snapshot from the leader over `GOSSIP_GET /v1/store-snapshot/<name>`, wholesale-replace | `main.rs:3617-3673`; serving arm `gossip.rs:286` | C | Becomes **pull-from-every-eligible-peer and merge each**. The byte-compare change gate survives (it is just a cheap "did anything change" test); `adopt` becomes `merge` |
| S2 | The 24 entries | `store_sync.rs:95-484` | mixed | See the table below |

Per-store merge decisions — this is the bulk of Stage 5 and each row is a
real design decision, not a mechanical rewrite:

| Store | Line | Merge rule | Notes |
|---|---|---|---|
| `browser_presence` | `:105` | **done** — LWW-element-set, tombstone dominance, 90s TTL | The template |
| `browser_admissions` | `:97` | LWW-element-set per endpoint id | **Do this first.** Carries the exact bug `browser_presence` was fixed for; already has tombstones + monotonic version, so the change is `adopt` → per-record merge |
| `acme_challenges` | `:470` | set union per name, TTL-expired | Prerequisite for E3. Consumed by `dnsserver.rs:425` — LE resolves through *any* advertised NS, so this store is already required to be fleet-wide correct |
| `audit` | `:374` | **grow-only set** keyed by entry id; never truncate | Wholesale replace of an append-only log actively destroys entries today under any second writer. See risk §7.5 — the honest answer may be fan-out query rather than full replication |
| `incidents` | `:197` | LWW per id + tombstone; auto-opened ids keyed by `(kind, subject)` per E4 | |
| `push` | `:210` | per-subscription LWW + tombstone; watermarks max-merge | Watermarks are monotonic |
| `notifications` | `:319` | grow-only per id + read-flag LWW | |
| `teams` | `:114` | per-team LWW; `plan` field removed in favour of B3's single tier record | |
| `billing` | `:133` | accounts derived from ledger; ledger grow-only keyed by deterministic id | **Last.** Depends on B1 |
| `projects` | `:172` | per-project LWW + tombstone | Largest record type; needs a `rev` field |
| `apikeys` | `:227` | per-key LWW + tombstone | Revocation must dominate — tombstone-wins, no resurrect |
| `webhooks` `databases` `domains` `integrations` `gitops` `docs` | `:240,253,266,279,293,306` | per-record LWW + tombstone | Mechanical once the record types carry `rev` |
| `identity` `enterprise` | `:336,349` | per-record LWW + tombstone | Secret-bearing; already ride peer-trust-enforced signed gossip |
| `waf` `router` `bot_policy` `ratelimit` | `:396,412,428,447` | per-rule LWW + tombstone, ordered by an explicit `priority` field, not map order | These are *enforcement* config — a merge that changes evaluation order silently changes enforcement. Needs an explicit total order in the record, not reliance on insertion order |
| `securelinks` | `:361` | **do not merge blindly** — node-affinity (the tunnel runs on the provisioning node) | Keep owner-scoped: the record's owner node is authoritative for it; other nodes hold a read cache |

### P. Everything else

| # | Responsibility | Where | Class | Replacement |
|---|---|---|---|---|
| P1 | **Raw port allocation/release**, fleet-global port space | `raw_ports.rs:28-42` (the hazard is written out in the module doc), 5 gates | **K by range sharding** | Each eligible node deterministically owns disjoint sub-ranges of `HIVE_RAW_PORT_RANGE` via `hrw_owner("rawport-shard-{i}", roster)`; a node allocates only from ranges it owns, so **no cross-node call is needed at all**. Claims still gossip as a read cache; the stamped `PortSpec::public_port` on the deployment record stays the durable copy (`raw_ports.rs` `adopt_record`). Hard rule: the shard governs **allocation only** — `lookup` and `release` must remain global, or a shard change strands live claims |
| P2 | Guardian departed-node reap | `main.rs:2547-2570` | K, key `"guardian-reap"` | The blast-radius guards in `guardian.rs:852-934` are the real safety, not the leader gate |
| P3 | Git poll reconcile | `git.rs:6164-6185` | **K, key = project** | A genuine parallelism win: today one node polls every git-sourced project serially. Per-project ownership spreads it. Dedup stays on the commit SHA (`state.rs:247`), which must be replicated (Stage 1) so a handover cannot double-deploy |
| P4 | Push VAPID keypair ensure | `push.rs:400` | K + never-overwrite | Losing the keypair is worse than a duplicate generation: store it with "first non-empty wins, never overwrite" so a concurrent second generation is discarded rather than replacing an in-use key |
| P5 | Push dead-subscription reap in `push_test` | `admin.rs:10144,10157` | C | Deletion is a tombstone in the `push` merge; any node may reap |
| P6 | Push subscribe/unsubscribe/SMS CRUD | `admin.rs:9775,9822,9921,9974,10037` | C | Follows the `push` store merge |
| P7 | Inference endpoint listing gate | `inference.rs:827` | **D** | Ownership is *already* HRW per project (`inference.rs:97`); this gate is vestigial and can be deleted in Stage 3 with no replacement |
| P8 | `build_get` / log reads leader fallback | `admin.rs:2128,2184,8705,8728,8796` | **O** | Resolve the **deployment host** from the gossiped deployment list; fall back to a bounded fan-out (first non-empty wins) across the eligible roster. Never a single global address |
| P9 | Browser artifact fetch fallback | `browser_artifacts.rs:952` | O | Owner = a deployment host referencing the descriptor. The re-verification of size + source BLAKE3 before serving proxied bytes is what makes fan-out safe here and must be preserved |
| P10 | Browser admission proxy reads/writes | `browser_admission.rs:1462,1486,1699,1791,1808` | O + C | Reads: owner-proxy. Writes: follow the `browser_admissions` merge. The relay-denylist clear (`fanout_deny_clear`) is already a fan-out and stays |
| P11 | Browser presence proxy read | `browser_presence.rs:408` | O | Store already merges; the leader proxy exists only to bootstrap an empty local view — becomes fan-out |
| P12 | Sandbox owner proxy | `sandboxes_api.rs:175` | O | `proxy_to_owner` already exists and is the correct pattern; the leader is only its fallback |
| P13 | `region_catalog` leader proxy | `admin.rs:627` | O | Fan-out; it is a read of gossiped data |
| P14 | Gossip arms gated on leader | `gossip.rs:1174,1202` | K | Retarget to the key's owner (these serve the raw-port allocate/release RPCs — they follow P1) |
| P15 | Boot + cluster-loop resolution calls | `main.rs:962,2504`, `state.rs:470` | — | Become ownership-roster damping ticks |

---

## 5. Staged migration

Design constraints on the staging, in priority order:

1. **No flag day.** Every stage is additive: the new path is built and observed
   while the old path stays authoritative, then the gate flips, then the old
   path is deleted in a *later* stage.
2. **Self-timed activation, not an operator switch.** Add `NodeInfo::cp_caps:
   u64` (bitmask, gossiped). Per the `disk_free_gb == 0 means UNKNOWN`
   precedent, `cp_caps == 0` means "pre-upgrade node," never "no capabilities."
   A stage's new path activates on a node only when **every healthy node in
   its registry** advertises that stage's bit. During a partial roll it is
   false everywhere; when the last node upgrades it flips true fleet-wide
   within one gossip round; if an old binary rejoins it flips back off —
   automatic rollback in the safe direction.
3. **The lease comes before the owner-selection change.** Because the fence is
   the safety and HRW is only the policy, Stage 2 lands the fence first. Then,
   during any window where nodes disagree about which selection rule is
   active, *both* the leader and the key-owner take the same lease, so the
   exclusion holds regardless of which rule each node used.
4. **Every stage's verification includes the AGENTS.md contrast test**: write
   through the public round-robin host, read back several times, and also test
   directly against a node still running the previous binary — the old node
   must still exhibit the old behavior. That contrast is the proof the change
   is real and that rollout skew is safe.
5. **No test files.** Every verification below is a live execution (curl
   against a running node, SSH to a fleet node, log-count across the fleet),
   per the repo's standing rule.

---

### Stage 0 — Measure and instrument (no behavior change)

**Change.** `NodeInfo::cp_caps` gossiped. A new operator endpoint `GET
/v1/cluster/ownership?key=<k>&class=<c>` returning, for the calling node: its
eligible-roster view (names + why each is in/out), the HRW winner, the live
lease + epoch, `cp_v2_active` per bit, and the legacy `control_plane_owner`
answer. Plus a rolling `owner_disagreement` counter: each node gossips its
computed owner for a fixed probe key and increments when a peer's answer
differs from its own.

**Verify.** Query the endpoint on all nodes for the same key and diff; run for
24h across at least one rolling restart. **Sustained disagreement is a stop
condition** — it means the roster is not converging and nothing downstream is
safe. Also record the Stage-4 gate metrics: p50/p99 mutation latency by origin
region, leader-unavailability minutes.

**Rollback.** Delete the endpoint. Nothing else changed.

**Note on the CI gate:** `cp_caps` is a new field on `NodeInfo`, a widely
constructed struct. Run `cargo test --workspace --no-run` before pushing —
`cargo check` does not build test targets, and this exact shape (two new
`NodeInfo` fields) went red across three CI jobs on 2026-07-31 after a clean
local check.

---

### Stage 1 — Replicate the leader-local state a failover already loses

Pure defect repair. **No leader gate is removed.** Worth shipping on its own
merits even if the project stops here.

**Change.**
- `PublishDamping` / `DelegationDamping` / `ReconcileGuards` /
  `api_ns_published` / `backoff` (`vercel_dns.rs:1821-1836`) move out of the
  task's stack into a replicated `DnsReconcileState` with an explicit merge
  (counters max-merge; circuit state by latest ts; `api_ns_published`'s
  `None`-is-safe-direction Hold semantics preserved).
- `BillingStore.meters` (`billing.rs:405`) enters `snapshot()`/`load()` and
  the `store_sync` billing entry, max-merged per counter.
- ACME account key: `$HIVE_DATA/acme-account.json` → sealed with
  `HIVE_SECRET_KEY` into a replicated store. **Carry the existing file** —
  read the old path if the store is empty and seal it in, never generate a new
  account when one exists.
- `git_poll_seen` (`state.rs:247`) → replicated grow-only set of
  `(project, sha)`, bounded by the deployed commit.

**Verify.**
- DNS: read `/v1/dns/stats` on the leader, note the damping counters, kill the
  leader mid-pass, confirm the successor's counters **do not reset to 0**.
- Billing: restart the metering node; the ledger delta for the tick after the
  restart must be comparable to the tick before, not the cumulative fleet
  total. This is the −$55 incident, reproduced as a test.
- ACME: issue a certificate from a node that has never been leader and confirm
  it uses the same LE account id (`/v1/acme` diagnostics or the LE account URL
  in the order). Confirm `secrets::audit_at_rest()` logs nothing.
- Git poll: force a leader change between a `git push` and the next poll tick;
  confirm exactly one build (`gw.list()` shows one new deployment, not two).

**Rollback.** Each item is independent; revert individually. The replicated
copies are additive and ignored by the old path.

---

### Stage 2 — `ownership.rs`: the primitive, with no callers

**Change.** One new module, ~200 lines:

```
eligible_roster(class) -> Vec<String>
    // healthy && peer_id.is_some() && (public_ip || public_ip6)   [cluster.rs:119-152, verbatim]
    // && cp_caps advertises the class's capability bit
    // && passed OWNER_HEALTHY_PASSES(2) consecutive healthy ticks  [publishable()'s K, vercel_dns.rs:54]
    // && not dropped by OWNER_UNHEALTHY_PASSES(2)                  [vercel_dns.rs:45]
    // class-specific extra filter (gpu / dns_validated / holds-the-container / hosts-the-tenant)

owner_for(key, class) -> Option<String>
    // lease::hrw_owner(key, eligible_roster(class))

try_own(key, class) -> Option<Fence>
    // None if mesh_health().isolated                               [state.rs:722, generalized]
    // None if owner_for(key, class) != self
    // leases.acquire_or_renew(key, self, region, ttl)
    // if class == Exclusive: quorum-confirm — POST /v1/ownership/confirm to
    //   the eligible roster, require ceil((N+1)/2) acks agreeing that self is
    //   the owner at this epoch, else release and return None
    // -> Fence { key, epoch }
```

`Fence` is `#[must_use]` and carries a `still_valid()` re-check that every
Class X caller must call immediately before each destructive external write.

**Why quorum, and what it costs.** §3.2's honest limitation means the local
lease alone cannot exclude a second acquirer during roster disagreement. The
quorum confirm is one bounded round of existing gossip RPC — no log, no
persistent state, no election, no leader. Its cost is that Exclusive work
becomes **unavailable during a partition**, which for DNS and ACME is the
correct direction (the codebase already prefers `Hold` over acting when
unsure, `vercel_dns.rs:441-455`) and for metering means usage accrues unbilled
until the partition heals — acceptable, and the direction billing already errs.

**Verify.** No production caller yet, so verification is synthetic and direct:
- Run `try_own("probe", Exclusive)` on a timer on every node for 24h across a
  rolling restart. Exactly one node reports `owned=true` at any instant; log
  every transition and confirm the count of simultaneous owners never exceeds 1.
- Partition test: drop the mesh on one node (firewall its iroh port from a
  fleet vantage, never from a laptop on the VPN — a VPN SYN-proxy reports
  closed ports as open). Confirm `try_own` returns `None` there while another
  node still owns.
- HRW stability: remove a non-owner node from the roster; confirm the owner
  does not change (this is the property that keeps a rolling restart from
  reassigning every key).

**Rollback.** Delete the module; nothing calls it.

---

### Stage 3 — Move the cheap, self-correcting loops off the leader

The loops where double execution is benign or self-correcting, so a mistake in
Stage 2 surfaces harmlessly.

**Change.** P7 (delete the vestigial gate outright), P2 (`"guardian-reap"`),
P3 (per-project git poll — also a parallelism win), P4 (VAPID, with
never-overwrite), P5 (dead-subscription reap).

**Verify per loop.** Pin `HIVE_CP_LEADER` to a node that is deliberately *not*
the HRW owner for the loop's key, then grep the loop's marker log line across
all nodes for a fixed window: the fleet-wide count must be exactly one per
tick, and it must be on the HRW owner, not the pinned leader. For P3 also
confirm two projects owned by different nodes both poll and both deploy.

**Rollback.** Per-loop revert to the leader gate; each is one line.

---

### Stage 4 — Owner-proxy reads: "ask the owner," not "ask the leader"

**Change.** The ten Class-O sites (P8–P13) resolve the key's owner —
deployment host for build/log reads, descriptor owner for artifacts,
admitting node for admissions, provisioning node for securelinks — and fall
back to a bounded fan-out across the eligible roster (first non-empty wins).
**The leader stays in the candidate list**; it is simply no longer the only
candidate. Additive by construction.

**Verify.** The AGENTS.md proof, per site: write through the public
round-robin host, read back N times through the public host (must succeed
every time), and read once against a node still on the previous binary (must
still succeed via the leader path). Specifically re-check the bug this class
protects: a deployment whose logs previously showed "Deployment started 0s
ago…, 0 lines, Waiting for logs…" forever must never return a hard 404.

**Rollback.** Per-site; each is a candidate-list change.

**This is the decision point.** Stages 0–4 fix live defects and reduce leader
dependence without changing any write path. Stages 5–9 are the expensive half.
Continue only if the Stage-0 metrics justify it.

---

### Stage 5 — Convergent stores, one at a time (the long pole)

For each store, in the risk order of the §4.S table: add per-record `rev` +
tombstone to the record type, write `merge(incoming) -> changed` modelled
literally on `browser_presence::adopt` (`browser_presence.rs:240-291`), change
the `store_sync` entry's `adopt` from `load()` to `merge()`, change the pull
loop from leader-only to all-eligible-peers, then — and only then — move that
store's routes out of R1/R2's mutation-forward table so its mutations serve
locally.

**Order.** `browser_admissions` (fixes a live latent bug) → `acme_challenges`
(unblocks E3) → `incidents` → `notifications` → `docs` → `webhooks` →
`integrations` → `gitops` → `databases` → `domains` → `apikeys` → `identity`
→ `enterprise` → `waf` → `router` → `bot_policy` → `ratelimit` → `push` →
`projects` → `teams` → `securelinks` (owner-scoped, not merged) → `audit`
(grow-only) → `billing` (last, after Stage 6).

**Verify per store — the test that fails today.** With the leader pinned to
node A: write record X on A through the public host, and write record Y
*directly on node B* (via the `x-hive-admin-forwarded` exemption so it lands
locally). Wait two pull intervals. Assert both X and Y are present on **every**
node. Today Y is silently destroyed on the next tick; that failing-then-passing
contrast is the proof. Then repeat with a delete of X from B and a concurrent
update of X on A — the tombstone must win and must not resurrect.

**Rollback.** Per store: restore `adopt = load` and put the routes back in the
forward table. Records that gained a `rev` field stay (additive, ignored by
the old path) — so a rollback loses convergence, never data.

---

### Stage 6 — The money path

**Change.** B1 (a)(b)(c) then B3, in that order. Deterministic ledger ids,
derived balances, replicated max-merged meter watermarks, then the single
versioned tier record replacing the two-halves plan write.

**Shadow period, mandatory.** Ship (a) in *shadow* first: compute the
deterministic id alongside the UUID, log both, charge on neither path
differently. Run it for **at least one full billing period** and confirm that
for every charge the deterministic id is unique per (tenant, window) and
stable across a leader change. Only then make it authoritative.

**Verify.** Deliberately run two meter loops (set
`HIVE_BILLING_COORDINATOR_NODE` on two nodes at once) and confirm: exactly one
ledger entry per (tenant, window) on every node after convergence; every
node's computed balance for that tenant is byte-identical; the tenant's
`can_deploy` lock state is identical fleet-wide. This is the test that fails
catastrophically today and must pass before the leader gate is removed. Then
re-run the −$55 reproduction from Stage 1 with two writers.

**Rollback.** (a) and (c) are additive and revertible. (b) — derived balances
— is a schema change to how the account is read; keep the stored counters
written in parallel for one release so a revert has something to read.

---

### Stage 7 — External-API zones

**Change.** E1 → Class K on key = zone name, Exclusive; E2 → Class K on key =
sorted identifier set, Exclusive; E3 consults the merged challenge set; E4
becomes Class I via stable incident ids.

Every Vercel write carries the fence, and `plan_writes` (`vercel_dns.rs:1130`)
re-checks `fence.still_valid()` immediately before each phase-0 delete,
aborting the pass if ownership was lost. The restore-on-failure transaction
and the create circuit (`vercel_dns.rs:1231-1265`) are unchanged — they are
the guards that make a *lost* pass recoverable, and the fence does not replace
them.

**Verify.**
- Vercel: force a handover mid-pass (stop the owner between phase 0 and phase
  1). Confirm the rollback/restore runs and is *verified* (not logged
  unverified), that no managed name ends the pass with neither addresses nor
  delegation (`alarm_dark_names` opens nothing), and that the successor's next
  pass converges. Separately: count Vercel API writes per pass before and
  after the change — the rate must be **unchanged**, not higher.
- ACME: use the **staging** directory. Drive two nodes to attempt the same
  bundle simultaneously; confirm exactly one order is created. Never run this
  against production LE — a failed test burns the 5/168h window for a week.
- E3: with two nodes holding in-flight challenges for different names, run a
  full sweep pass and confirm neither node's live TXT is deleted.

**Rollback.** Restore the leader gate on `spawn_reconciler` / `spawn_acme`;
the fence machinery stays and is simply unused.

---

### Stage 8 — Raw ports

**Change.** P1: deterministic disjoint range shards, allocation from owned
shards only. `lookup` and `release` stay global.

**Verify.** Trigger two simultaneous first-deploys of different projects
targeting different nodes; confirm distinct ports and no bind conflict. Then
stop a node: confirm its shard's existing claims still resolve (they are
stamped on the deployment records and re-adopted by `adopt_record`) while new
allocations move to surviving shards. Then restart it and confirm no
reallocation of live claims.

**Rollback.** Restore `allocate_raw_ports_coordinated`'s leader forward.

---

### Stage 9 — Delete the leader

Entry criterion: `is_control_plane_leader()` / `control_plane_leader()` call
sites outside `cluster.rs` reach **zero**, and R1/R2's mutation-forward table
is empty.

**Change.** The forward branches become no-ops; `Cluster` reduces to the epoch
(kept as a global fencing generation) plus the `ClusterStatus` wire shape,
which the dashboard reads — preserve the field names, set `leader: ""`,
`consensus: "ownership"`. `HIVE_CP_OWNER_CHAIN`, `HIVE_CP_LEADER`,
`HIVE_DNS_LEADER_NODE`, `HIVE_BILLING_COORDINATOR_NODE` become no-ops that log
a deprecation warning if set, and are removed from ansible one release later.

**Verify.** With `HIVE_CP_OWNER_CHAIN` unset fleet-wide, run the full
dashboard mutation surface against each node *directly* (bypassing round-robin
DNS) and confirm every mutation applies locally and converges everywhere
within one pull interval. Then the AGENTS.md contrast one final time against
an old-binary node — which at this point must **fail**, proving the migration
is complete rather than dual-pathed.

---

## 6. Taubyte: what transfers, what does not

Taubyte (`tau`, pinned `f5c9c9c3`) is worth reading, and mostly for reasons
opposite to how it is usually cited.

### Transfers

1. **The scoping insight, which is the whole lesson.** In the entire public tau
   repo exactly one service requires Raft — `patrick`, the CI job queue
   (`pkg/specs/common/raft.go:4-6`). The string "leader" appears nowhere in
   `services/`, `clients/`, `p2p/`, `core/` or `pkg/` outside `pkg/raft/`.
   Everything else — config/TNS, auth, storage placement, DNS/registry, the
   whole serving path — is coordination-free. And the *reason* Raft exists
   there is precise: before it, patrick did job mutual-exclusion with a lock
   key in an LWW CRDT store, i.e. read-then-write over an eventually-consistent
   store, which tau's own AGENTS.md says "guarantees nothing under
   concurrency." Raft solves exactly one problem CRDTs cannot: *which builder
   gets this job, exactly once*.
   **Applied here:** hive's irreducible set is E1 (Vercel zone), E2 (LE
   orders), and — until Stage 6 makes it idempotent — B1 (metering). That is
   three, and two of them are constrained by *external* systems, not by our
   own data model. Everything else in the §4 map is Class C, I, O, or D.
2. **The schema discipline, verbatim.** From tau's AGENTS.md: one key per
   entry, never a contended key; put the discriminator in the key path; make
   conflict visible as two entries and resolve it deterministically on read,
   so "every node computes the same answer from the same replicated state,
   with no coordination." That sentence is Stage 5's specification. It is a
   *schema* rule, not a protocol — which is why it is the transferable part.
3. **Honest naming of at-least-once.** Patrick's dequeue pops from Raft then
   writes the assignment to the CRDT store on a detached bounded context,
   with `ReannounceJobs` re-pushing stale assignments — net semantics
   "at-least-once with a duplicate-build window," and the code says so
   (`services/patrick/jobs.go:34`). Hive should be equally plain: after Stage
   6, metering is at-least-once *and convergent*, which is strictly better and
   should be documented as such rather than claimed as exactly-once.
4. **Its honest caveat about single-holder writes:** "A write acknowledged by
   a single node can die with that node if no other holder had the database
   open." Hive has the same exposure via `db_replicate::fanout_all`
   (best-effort, no acks consumed, no backfill for nodes that were unhealthy),
   and Stage 5's all-peers pull is the anti-entropy tau's hoarder K=2 barrier
   substitutes for.

### Does not transfer

1. **The Raft fork itself.** Hive has no libp2p, no `go-libp2p-gostream`, no
   HashiCorp Raft. Adding a consensus log means operating a **third**
   replication substrate on top of GuardianDB/iroh-docs and cr-sqlite, for a
   capability HRW + fenced lease + quorum-confirm already provides at a
   fraction of the maintenance surface. Net-larger surface for zero new
   capability is exactly the trade this repo's own rules reject.
2. **Their split-brain healer — actively dangerous, do not import.** On
   detecting split-brain, Taubyte-Raft picks a winner by `MemberCount` →
   `LastIndex` → lexicographic leader id (`healing.go:383-408`), merges FSM
   state per key by LWW, and makes the **loser delete its Raft log and stable
   store and wipe its snapshot dir** (`healing.go:535-602`). Three specific
   problems: (a) it trades away Raft's core safety guarantee — a committed
   entry survives only if it wins a per-key compare; (b) the "Lamport
   timestamp" is a per-node apply counter (`fsm.go:78,115`), so a higher value
   means "that partition applied more commands," not "later"; (c) `healAck` is
   **unauthenticated** (`stream.go:606-637`, acted on unconditionally at
   `healing.go:81-89`), so any holder of the swarm PSK can make any patrick
   node delete its log. Hive's `HIVE_PEER_TRUST` allowlist posture is the
   opposite and must stay.
   The deeper point: hive does not need this healer because hive is not
   claiming linearizability in the first place. Converging by merge is fine
   *when you never promised otherwise*; it is a safety violation only when
   bolted onto a protocol that did.
3. **Their timing constants, and the conclusion they imply.** Tau runs
   `HeartbeatTimeout 15s / ElectionTimeout 30s` "tuned for worldwide
   distributed clusters" (`pkg/raft/config.go:34-41`) — 15–30× vanilla — and
   its split-brain healer engages only after 180s of leaderlessness. Hive's
   WAN reality is the same (a *successful* sj health probe measured 7462ms).
   So if hive did adopt Raft, its timeouts would have to be equally huge,
   giving tens of seconds of write unavailability per failover — **worse than
   today's owner-chain failover**, which is bounded by the 3s cluster loop.
   Tau's own constants are the argument against copying tau.
4. **Membership.** Tau's `RemoveServer` is never called by any production path
   (only tests), so its voter set grows monotonically and dead voters inflate
   quorum forever; and `joinVoter` adds a brand-new empty node as a full
   **Voter** with no non-voter catch-up stage, from a **request-body-supplied
   peer id** (`stream.go:484-492`) — any swarm peer can have an arbitrary id
   added as a voter. Hive's roster is churn-native (30s TTL drop in
   `NodeRegistry::nodes()`, `hive-edge/src/region.rs:561-574`); grafting a
   monotonic quorum onto it would be a regression, and grafting tau's
   unauthenticated join onto `HIVE_PEER_TRUST` would be a security one.
5. **Discovery/bootstrap.** The "native discovery rather than config files"
   claim is true only above libp2p — a tau node still bootstraps from a
   `peers:` multiaddr list in YAML. Hive's `HIVE_BOOTSTRAP_PEERS` is the same
   shape with harder documented hazards (bare ids don't converge; the private
   addrs in `peer_iroh.json` aren't dialable seeds). Nothing to learn here.
6. **Timing of the claim.** Both posts that describe tau's Raft were published
   within a day and three weeks of `pkg/raft` first landing (`1e5036ff`,
   2026-01-29), on a repo dating to 2023-07. The "leaderless architecture" they
   implicitly praise is the *other 95%* of tau, which predates Raft entirely —
   which is another way of saying the transferable lesson is §6.1, not the
   Raft package.

---

## 7. Irreducible risks

These do not go away with better engineering. They are the price of the
design and must be accepted explicitly or the project should not start.

**7.1 HRW agreement is only as good as roster agreement, and hive's health
verdict is per-observer *by design*.** AGENTS.md: "A health verdict is
per-OBSERVER, so peers legitimately disagree and a probe-side fix only helps
the node running it," with a live measurement of one peer calling
fc-virginia-2 unhealthy while three others called it healthy — and the
dissenter being wrong. No hash function fixes disagreement about its input.
Damping (K=2 in and out) and quorum-confirmed fences bound the damage; they do
not eliminate it. The residual: during roster disagreement, Exclusive work is
**unavailable**, and non-Exclusive work may run twice.

**7.2 Every node still holds 100% of control-plane state, and after this it
holds more.** Revisions, tombstones, and per-column Lamport versions are pure
addition. If the actual motivation was per-node data volume, this project does
not deliver it and namespace sharding (`persist::namespaced`) does. Said
plainly so nobody discovers it at Stage 6.

**7.3 Wall-clock dependence survives for anything left on GuardianDB.**
iroh-docs resolves by `entry.timestamp()` with strict `>`; cr-sqlite never
consults its `ts` column. Any store that stays on GuardianDB keeps a
clock-skew failure mode no amount of leaderlessness removes, and fleet NTP
becomes a correctness dependency for it. Choose per store, deliberately;
prefer cr-sqlite for anything a human would call a record.

**7.4 Deleting is the hard part of a CRDT, and tombstone retention is an
unbounded-growth-vs-resurrection trade with no third option.** A node absent
longer than the tombstone retention returns and resurrects deleted records —
including revoked API keys and deleted projects. Proposal: retain **30 days**
(matching `HIVE_BROWSER_DB_INERT_GRACE_SECS`) and treat a node absent longer
as requiring a **state wipe + re-seed**, never a merge. That must become an
operational rule with a check at boot ("my newest tombstone is older than
retention ⇒ refuse to merge, request re-seed"), not a footnote — and note the
whole class inverts the current safety posture, where `adopt` declining an
empty payload protects a follower from a booting leader.

**7.5 The audit log cannot be LWW and probably cannot be fully replicated.**
It is grow-only and legally meaningful; a real merge means every node
accumulates every entry forever. The honest choice is **fan-out query** —
audit stops being fleet-complete on any single node and reads gather from the
eligible roster — which changes the audit API's contract and its failure mode
(a partitioned node yields an *incomplete* answer, which must be surfaced as
incomplete, never as empty). Decide this before Stage 5 reaches `audit`.

**7.6 External APIs will not honour our fencing token.** Vercel and Let's
Encrypt have no concept of our epoch. A fence stops *us* from issuing the
second write; it cannot stop a write already in flight from an evicted owner
from landing *after* the new owner's. Mitigation is bounding, not
elimination: lease TTL short relative to the HTTP timeout, `still_valid()`
re-check immediately before each destructive step, and an idempotent
recompute-and-diff plan (which `plan_writes` already is). Residual: a
pathological interleaving can still produce a name with neither addresses nor
delegation, which is why `alarm_dark_names` and the restore-on-failure
transaction must survive the migration untouched.

**7.7 Money has no undo.** Even with deterministic ledger ids, a defect in the
id derivation silently merges two genuinely distinct charges into one
(under-billing, invisible) or splits one into two (double-billing, visible and
customer-facing). The derivation must use data that is truly unique per
charge, and Stage 6's shadow period is not optional.

**7.8 Enforcement config is not ordinary data.** `waf`, `router`,
`bot_policy`, `ratelimit` decide what traffic is allowed. A merge that changes
*evaluation order* changes enforcement without changing any rule. These
records need an explicit total order field before they can be merged at all —
relying on map iteration order after a merge is a security bug wearing a
convergence costume.

**7.9 The work is large and the visible payoff is zero.** 66 call sites, 24
stores, ~17 record types needing `rev` + tombstone, a new ownership module, a
quorum path, a shadow billing period, plus every stage's live verification
against a real fleet. There is no user-facing feature at the end — only lower
mutation latency and a smaller blast radius. Stages 0–4 stand on their own
(they repair defects that bite today); Stages 5–9 need the Stage-0 metrics to
justify them.

**7.10 Linux-gated and test-target blind spots.** Per AGENTS.md: a macOS
`cargo check` compiles neither `cfg(target_os = "linux")` code nor
`#[cfg(test)]` targets. Stage 0's `NodeInfo` field and Stage 2's new module
both touch widely-constructed structs. Every stage must run `cargo test
--workspace --no-run` locally and a real build on a fleet node of each glibc
group (2.38: bkk, hk, the five GPU/CVM nodes; 2.39: va/va2/va3/sj/sj2/sp/fr)
before rollout — verified with `scripts/audit-runtime-versions.sh`, never from
memory.

---

## 8. AGENTS.md amendments this design implies

To be written when the corresponding stage lands, not before:

- **Stage 4/5:** the round-robin-reads-vs-leader-forwarded-writes section
  changes shape. The new rule: *every new endpoint declares its class* — C
  (merged store, mutations serve locally), K (key-owned, forwarded to
  `owner_for(key)`), X (externally serialized, requires a fence), or O
  (owner-proxy read with fan-out fallback). "Ask the leader" stops being an
  available answer, and an endpoint with no declared class is a review block.
- **Stage 5:** a store added to `store_sync::REGISTRY` must ship a `merge`,
  never a `load`. Wholesale replace becomes a banned shape with
  `browser_presence.rs:216-239` as the cited postmortem.
- **Stage 5:** tombstone retention (30 days) and the absent-longer-than-
  retention ⇒ wipe-and-re-seed rule.
- **Stage 6:** ledger ids are deterministic; balances are derived; no stored
  mutable money counter may be written read-modify-write.
- **Stage 9:** `HIVE_CP_OWNER_CHAIN` / `HIVE_CP_LEADER` /
  `HIVE_DNS_LEADER_NODE` / `HIVE_BILLING_COORDINATOR_NODE` are removed; the
  ansible inventory drops `hive_cp_leader`.
