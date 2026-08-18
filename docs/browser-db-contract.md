<!--
Contract row: bn-browser-db-ownership-contract (.gm/prd.yml).
Implemented against by: bn-browser-fleet-crr-exchange (the live browser↔fleet
exchange). Builds on: bn-impl-sqlite-automerge (browser worker slice,
crates/hive-browser/www/sqlite/), bn-impl-fleet-crr-peer (fleet CRR seam,
crates/hive-crsql), browser-admission-derived-capabilities +
browser-function-artifact-delivery (the admission/capability and owner-proxy
precedents this contract ties to). Written as the concrete contract the
exchange implements, not a design essay: every rule below is stated so it can
be checked in code.
-->

# Browser Database Contract — ownership, ACL, caps, retention, naming

A deployment may opt its **project** into ONE browser-replicated cr-sqlite
database. Browsers holding a live, server-derived grant replicate it through
the cr-sqlite CRR protocol (`hive_crsql`'s ChangeBatch seam); the fleet keeps
the durable copy. This document is the whole ownership/retention story the
exchange row implements — the opt-in shape, who may read/write/replicate, the
size caps on both sides, what happens to every replica when admissions end,
and exactly which existing replicated store carries which metadata.

Out of scope here (the exchange row's own design): the wire transport and
ALPN, sync scheduling / anti-entropy cadence, fleet-fleet replication
topology, and the code that wires each rule in. The CRR semantics themselves
(per-origin-site durable watermarks, HCB1 batches, gap/replay, bounded
transactional apply) are `crates/hive-crsql/src/lib.rs`'s documented contract
and are named here, never redefined.

---

## 1. Opt-in shape: fluid.json `browser_db`

A project gets a browser database ONLY by opting in through a **top-level**
`browser_db` block in `fluid.json` — the presence-is-the-opt-in discipline of
`functions[].browser`, at the deployment-descriptor level:

```json
{
  "project": "my-app",
  "functions": [ ... ],
  "browser_db": {
    "max_bytes": 134217728,
    "max_value_bytes": 2097152,
    "public_read": false,
    "schema": [
      { "name": "items",
        "ddl": "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY NOT NULL, label TEXT);" }
    ]
  }
}
```

Schema (`fluid_core::BrowserDbPolicy`; every field optional, `0`/absent =
platform default, over-ceiling clamps with a build-log note):

| field             | type | default    | ceiling     | meaning |
|-------------------|------|-----------|-------------|---------|
| `max_bytes`       | u64  | 64 MiB    | 1 GiB       | Per-replica total size cap, enforced on BOTH the browser OPFS copy and every fleet-side replica file. |
| `max_value_bytes` | u64  | 1 MiB     | 16 MiB      | Single change-value payload cap (one `crsql_changes` `val`), enforced at the sync boundary in both directions. |
| `public_read`     | bool | `false`   | —           | Allow PUBLIC-scope admissions a READ-ONLY replica (§3). |
| `schema`          | list | `[]`      | —           | `{name, ddl}` per CRR-tracked table. cr-sqlite v0.17 does NOT replicate schema inside `crsql_changes`, so both replica halves derive it from THIS spec: the fleet reconcile applies each `ddl` + `crsql_as_crr(name)` to the replica file; the admission `db` capability hands the same list to the browser verbatim. `name` must be identifier-shaped (`[A-Za-z_][A-Za-z0-9_]*`); author each `ddl` idempotent. Empty schema = no tracked tables (exports are empty; applies of unknown tables fail loudly, never silently diverge). |

Validation discipline — the same as `BrowserPolicy`'s, one level up:

- A **mistyped** block (string where a number belongs, etc.) fails the deploy
  at the `Manifest::from_json` boundary — a loud parse error, never a silently
  dropped opt-in.
- **Over-ceiling** values clamp with a note (`BrowserDbPolicy::resolve()` →
  `ResolvedBrowserDbPolicy { max_bytes, max_value_bytes, public_read, notes }`)
  — the `ContainerLimits::for_container` / `BrowserPolicy::resolve` convention.
  A `max_value_bytes` larger than `max_bytes` clamps to `max_bytes` with a
  note (a single value that could never fit the database is a tenant authoring
  error, surfaced, not fatal).
- Resolution is infallible by design: no tenant-authored value is dangerous to
  the platform, so there is nothing to hard-reject beyond serde types. The
  deploy/build path resolves the block and logs `notes`; it MUST NOT warn-and-
  drop the opt-in (the browser-function rule: the fleet never serves a state
  the tenant did not ask for while believing it did).

**Cardinality: one logical database per project.** One block per fluid.json;
no per-function blocks, no named databases. Database IDENTITY is the project
(§5, §6); the opt-in spec and every grant are resolved against a specific
Ready deployment's descriptor (§3). Multi-database-per-project is explicitly
deferred (§9).

## 2. Where the spec lives (replicated-store map)

The spec is deployment-scoped replicated state, exactly the shape of the
artifact descriptors. This map is normative — the exchange reads metadata
from these stores and nowhere else:

| metadata | home | how it replicates |
|----------|------|-------------------|
| Raw opt-in spec (`BrowserDbPolicy`) | `fluid_core::Manifest::browser_db` | Rides `DeployRecord.manifest` in the gateway deployment store; persisted in `PlatformSnapshot.deployments` (node-local persistence); stamped VERBATIM onto `DeploymentInfo::browser_db` (fluid-gateway `view_of`) and replicated fleet-wide by the `/v1/fleet-deployments` gossip into every node's `peer_deployments`. |
| Resolved caps | `BrowserDbPolicy::resolve()` | NOWHERE — derived deterministically at each point of use (raw-spec-replicated, validated-at-consume: the `InferenceSpec` precedent). Every consumer applies its own binary's defaults/ceilings, so mid-rollout peers can never disagree about a stored resolved value. |
| Per-browser grants | `BrowserAdmission` (browser_admission.rs) | The existing `browser_admissions` entry of `store_sync::REGISTRY`: leader-owned, snapshot-replicated to every follower (authoritative-empty included). The exchange extends the admission record + admit/renewal capability with a `db` block (§3) — server-derived from the descriptor above, never client input. |
| DB bytes, site ids, sync watermarks | Node-local replica files (§6) | NEVER `PlatformSnapshot`, NEVER `store_sync`, NEVER a gossip snapshot arm (the `dns_geo.json` / browser-artifacts precedent). Replication is the CRR protocol and ONLY the CRR protocol. `crsql_site_id` and the `crsql_db_versions` watermarks live INSIDE each replica file, maintained by the extension. |
| Ownership (tenant of a project) | `ProjectStore` / deployment record tenant | Existing resolution: `record_tenant()` on the deployment record — the exact call admission already makes. |

Consequences the exchange must honor:

- **Mid-rollout direction.** A pre-upgrade peer builds `DeploymentInfo`
  without `browser_db` (`serde(default)` on the new field), so deployments
  hosted on old binaries present as NOT opted-in to upgraded nodes: no grants
  are issued for them. That is the designed failure direction — absent
  capability, never wrong capability.
- **Round-robin rule.** No HTTP GET ever serves DB bytes or DB metadata from
  node-local state: metadata reads come from the gossip-replicated
  `peer_deployments` view (already fleet-wide), bytes flow only through the
  mesh exchange protocol (§5). There is therefore no owner-proxy arm to add —
  and none may be added; a file copy is not a merge.

## 3. Ownership & ACL — who may read, write, replicate

Everything is pinned to the admission/capability model
(browser-admission-derived-capabilities): grants are leader-issued,
tenant-pinned, short-leased, server-derived, and they die with the admission.

- **The grant rides the admission.** When an admitted deployment's descriptor
  carries `browser_db`, the admit/renewal response's capability block gains a
  server-derived `db` section, one atomic snapshot with the rest of the
  capability: `{ tenant, project, access, max_bytes, max_value_bytes,
  db_file, schema, sync_peers, expires_ms }`. `db_file` is the
  platform-derived replica name (§6); `access` is `read_write` or
  `read_only`; `schema` is the spec's table list verbatim (the browser's
  only schema source — §1); `sync_peers` is the small list of healthy fleet
  `{endpoint_id, addr_json}` the browser may dial for sync rounds (the
  `trusted_callers` discipline: server-derived from the live registry,
  health-filtered, never client input); `expires_ms` is the admission's own
  expiry. The admission record's `db` field is the replicated grant the
  exchange checks.
- **Tenant pinning.** The grant's tenant is the authenticated session's tenant
  (`admin::norm(claims.tenant)`), and the deployment must resolve under THAT
  tenant to a Ready deployment carrying the block — the same descriptor
  resolution admission performs for artifacts (`descriptor_for`'s shape: local
  records, then the gossiped peer view). A sync request against a foreign
  tenant's project gets the SAME refusal as an unknown project — no existence
  leak across tenant boundaries (the artifact GET's 404-for-both rule).
- **Access classes.**
  - `BrowserScope::Team` (a tenant member's browser): `read_write` — the
    browser may export its changes to the fleet and apply fleet changes. This
    is the default and the whole point of a CRR: browsers are writers.
  - `BrowserScope::Public` (anonymous donor): `read_only`, and ONLY when the
    deployment's spec has `public_read: true`. Read-only means the fleet never
    applies a change originating from that grant — export (`changes-since`)
    only, apply refused. A public donor must never write tenant data,
    regardless of the toggle. The existing admission rule stands unchanged:
    Public scope itself requires a team owner/admin session.
- **Exchange-side re-check.** The fleet peer serving a sync request
  re-validates the grant against its OWN replicated admission-store view
  (live, unexpired, covering exactly this tenant+project, access class
  permitting the requested direction) AND re-checks the deployment descriptor
  still carries the block — the `proxy_to_owner` re-check precedent: a grant
  presented by a browser is a hint, the server's own stores are the decision.
- **Revocation and expiry are the admission's own.** An explicit revoke
  (DELETE) and a lease expiry invalidate the DB grant at the same instant,
  with no separate DB teardown path on the fleet side — the exchange simply
  finds no live grant. The browser-side consequences are §5.
- **Who may write, v1 totality:** admitted browsers, per the classes above.
  Direct writes from the project's server-side functions (an injected
  connection string / host op) are NOT in this contract (§9).

## 4. Size & quota caps — browser OPFS side AND fleet side

| cap | value | enforced where | failure shape |
|-----|-------|----------------|---------------|
| Per-replica total size | spec `max_bytes` (default 64 MiB, ceiling 1 GiB) | BOTH sides: the browser worker before/after apply and on local growth; the fleet peer before committing an apply batch. | Typed quota refusal (`QuotaExceededError`-shaped on the browser; a `quota_exceeded` sync error fleet-side). The applying batch ROLLS BACK whole (`apply_batch` is one transaction) — the replica keeps its prior state. NEVER truncate or evict rows to fit: eviction from an LWW replica is silent permanent divergence, the one failure mode a CRR must not have. |
| Single change value | spec `max_value_bytes` (default 1 MiB, ceiling 16 MiB) | The sync boundary, both directions: a change whose `val` payload exceeds the cap is refused export (origin keeps it locally, error names table/pk) and refused apply. | Loud sync error; the oversized value simply never replicates. Never truncated (§ BrowserDbPolicy docs). Bigger payloads belong in the content-addressed asset store, referenced from a row. |
| Batch bound | `hive_crsql::DEFAULT_MAX_BATCH_CHANGES` (256) | Already the seam's export chunking; a single origin commit larger than it still travels as one oversize batch (atomicity wins). | Unchanged by this contract — named so the exchange does not invent a second bound. |
| Browser origin quota | Browser platform's OPFS quota | The worker already surfaces it typed (`open-error` / `sql-error` with the real DOMException name; persist-first, `navigator.storage.persist()` reported honestly). | Unchanged — a quota/IO failure is surfaced, never retried-quietly, never swallowed. |

Fleet-side files live under `$HIVE_DATA` on each exchange-participating node
and are bounded per-DB by `max_bytes`; a per-TENANT aggregate fleet cap is
deferred (§9) — v1's bound is per-database × opted-in projects, and node disk
pressure remains governed by the existing placement/GC invariants.

## 5. Retention & eviction — every replica is a cache of record

**Authoritative copy: the converged fleet replica set.** Every browser OPFS
copy is a cache of record — it counts toward replication factor ZERO (the
asset-store rule: browser storage is demand-side only), and the fleet never
depends on a browser replica being reachable. The CRR wrinkle, stated
honestly: writes a browser made but never synced exist only in that browser;
losing the replica loses exactly those writes. The worker's persist-first
discipline plus sync-on-write is the mitigation; nothing in this contract
pretends a vanished tab's unsynced writes survive.

**Browser side (OPFS copy of `db_file`):**

- **Admission revoked** → the browser wipes its OPFS replica for that project
  (after a best-effort final push when connectivity allows). Revocation is an
  explicit security action; the copy must not linger readable.
- **Admission expired** → the replica is SEALED: no exchange, and the app-
  facing handle closes until a successful re-admission. Re-admission resumes
  incrementally from the replica's durable per-site watermarks
  (`crsql_db_versions` — they live in the file, so resume costs only the gap).
  A re-admission that fails with `deployment_not_ready` / forbidden is treated
  as revocation → wipe.
- **Browser churn** (tab vanishes, browser wiped, Safari's 7-day eviction) →
  nothing to do, by construction: the fleet replica set is unaffected, and a
  fresh browser simply syncs from watermark 0 (a wiped CRR peer is safe by
  construction — a new `crsql_site_id`, a full export, LWW convergence). Stale
  per-site watermark rows left behind in other replicas are inherent CRR
  bookkeeping, not garbage this contract reclaims.
- **Spec decrease** (`max_bytes` lowered across a redeploy): replicas over the
  new cap stop accepting applies (typed quota refusal) until they fit; nothing
  is deleted to force compliance.

**How the production run-node worker applies those rules**
(bn-run-node-db-sync-wiring, `ui/public/run-node-worker.js`): the db lane
rides the admission renewal spine — it never keys a destructive action off a
sync refusal alone, because a transient dial failure is indistinguishable
from revocation at that layer. Concretely: `stop()` is the local revoke
(best-effort final push while the grant is live, then wipe); a terminal
admission denial or a successful renewal whose capability lost the `db`
block wipes without a push (the grant is already gone server-side); a
refused sync round or a failed non-terminal renewal SEALS the lane and kicks
re-admission — resume is incremental from the durable watermarks. Sync
cadence in the production worker is the exchange glue's on-write kick plus a
30s periodic round against the granted `sync_peers` (a converged round is
one small frame per peer), with fleet-initiated rounds served through the
node's responder arm at any time. Hosting constraint, measured live: a
SharedWorker has no `Worker` constructor (Chrome, 2026-08), so the sqlite
DedicatedWorker is page-brokered over the worker's control protocol
(`dbWorkerRequest`/`dbWorkerPort`/`dbWorkerDone`) in that context and nested
directly in the dedicated-worker fallback; the OPFS replica survives the
broker dying (page reload) and is re-opened with watermark resume. Lane
state is surfaced on the worker status object (`db`: project, access, state,
persisted, peers, lastSyncMs, sites/siteVersion, error) and rendered as a
single status line on the dashboard's `/run-node` page — nothing renders
when the admitted project has no `browser_db` block.

**Fleet side (one replica file per participating node, §6):**

- **Redeploy keeping the block** → same database (identity is the project);
  grants re-issue against the new Ready deployment; watermarks and site ids
  continue.
- **Deployment deleted / superseded** → grants against it die at lease expiry
  (≤ the admission lease, minutes); the DATA is unaffected — the database
  belongs to the project.
- **Block removed from the project's latest Ready deployment** → new grants
  stop immediately; the fleet replica files are retained INERT for a 30-day
  grace window (one bad deploy must not nuke production data), then reaped by
  the exchange's GC.
- **Project deleted** → the replica files are deleted, in the same GC pass.
- **GC guards — the `browser_artifacts::gc` / `gc_rootfs_images` discipline
  verbatim:** the keep-set is every live project (inert-grace files kept until
  their grace deadline); an EMPTY keep-set refuses the pass outright; a reap
  set over `HIVE_BROWSER_DB_GC_MAX_REAP_FRACTION` (default 0.5) refuses; only
  files older than `HIVE_BROWSER_DB_GC_GRACE_SECS` (default 600) reap. A blast-
  radius check is the difference between a bug and an unrecoverable one.

## 6. Naming & authorization — platform-controlled identifiers only

The AGENTS.md tenant-volume-isolation invariant applies to every file layout
this contract creates: **a tenant-controlled string never becomes a path
component**; names are constructed from a platform-owned template.

- **Fleet replica file:**
  `$HIVE_DATA/browser-dbs/hive-browserdb-{sanitize_tag(project)}.db`
  — the `container_volume_cfg` precedent exactly: `sanitize_tag` lowercases,
  maps every character outside `[a-z0-9._-]` to `-`, collapses repeats, trims
  leading/trailing separators, so the result is always a plain file name even
  for a project called `/` or `../../etc`. Project names are globally unique
  in the platform data model (the ProjectStore is keyed by project name — the
  same invariant `hive-vol-{project}` already relies on).
- **Browser OPFS replica file:** `/hive-crsql/hive-browserdb-{sanitize_tag(project)}.db`
  — the same name under the worker's existing `AccessHandlePoolVFS` directory.
  The server-derived `db_file` field of the capability (§3) carries this name;
  the browser exchange glue opens exactly it and nothing derived from any
  other input. OPFS is origin-shared across tenants the donor serves, which is
  precisely why the name is platform-derived and grant-scoped rather than
  anything the page supplies.
- **Authorization of file access:** the exchange derives the file name from
  the GRANT it resolved (tenant+project → template), never from a
  wire-supplied name or path. The sync request carries a `db_file` field,
  but it is a grant IDENTIFIER, not a path: the browser echoes the name its
  own server-derived capability carried, the fleet compares it against the
  name it derived from the live grant, and a mismatch is the same refusal
  as no grant (a stale capability — the endpoint re-admitted to a different
  deployment — can never contaminate another project's replica). Only the
  server-derived name ever reaches the filesystem — the same rule as the
  artifact GET validating its digest before it ever becomes a path
  component.
- **Database identity vs file name:** the CRR identity of a replica is its
  `crsql_site_id` INSIDE the file, and convergence is by per-site watermarks —
  the file name is only an address. A file therefore renames safely and a
  project's database survives every redeploy with its site id intact.

## 7. Failure modes (each with its designed direction)

- **Pre-upgrade peer / missing field** → presents as not-opted-in; no grants
  (§2). Never wrong grants.
- **Leader failover mid-lease** → grants survive: the admission store is
  snapshot-replicated and the new leader adopts it; renewals re-derive the
  same `db` capability from the descriptor.
- **Sync gap** (a batch chaining from ahead of the receiver's watermark) →
  `SyncGap` refusal, nothing written, re-request from the durable watermark —
  already the seam's semantics; the exchange surfaces it, never papers over it.
- **Quota exceeded** → typed refusal + whole-batch rollback, both sides (§4).
- **Fleet node loss** → remaining replica files still converge; a replacement
  node opens an empty replica and backfills from watermark 0 via the same
  protocol. No restore path, no snapshot copy.
- **`ts` obligation** → the fleet side sets `crsql_set_ts` per write
  transaction exactly as the browser worker's `set-ts` op does; a write
  without it records `ts='0'` into every peer's clock tables (the hive-crsql
  warning, binding on the exchange's fleet half too).
- **0.5-RTT / unauthenticated first flight** → no apply, no export, no grant
  check bypass on first-flight data — the proposal's §2.1 rule, restated here
  because DB mutation is exactly the side-effectful op it names.
- **Verification contrast** — per the AGENTS.md round-robin rule, the exchange
  row proves this contract by writing through a grant and reading back across
  nodes, AND against a node running the pre-change binary, which must still
  refuse (no grants). The contrast is the proof.

## 8. The type contract (fluid-core, landed with this document)

`fluid_core::BrowserDbPolicy` — additive, `serde(default)` everywhere;
`Manifest::browser_db: Option<BrowserDbPolicy>`;
`DeploymentInfo::browser_db: Option<BrowserDbPolicy>` (stamped verbatim by
fluid-gateway's `view_of`); `BrowserDbPolicy::resolve()` →
`ResolvedBrowserDbPolicy`; platform constants
`BROWSER_DB_MAX_BYTES_DEFAULT/MAX`, `BROWSER_DB_VALUE_MAX_BYTES_DEFAULT/MAX`.
The exchange row added one additive field: `BrowserDbPolicy.schema:
Vec<BrowserDbTable>` (`{name, ddl}`, default empty — §1; validated at
`resolve()`, never hard-failed). No pre-existing type changed shape;
pre-upgrade peers serialize and parse exactly as before.

## 9. Deliberately deferred

Landed with the exchange row (bn-browser-fleet-crr-exchange): the wire
transport (one `Op::CrrSync` op on the existing `hive/browser/0` ALPN, both
directions of initiation — browser-dialed rounds via the wasm `crrSyncOn`,
fleet-dialed rounds via `hive_p2p::BrowserPool::crr_sync`); the admission
record + capability `db` block; the fleet reconcile/GC loop and its env
knobs (`HIVE_BROWSER_DB_RECONCILE_SECS`,
`HIVE_BROWSER_DB_INERT_GRACE_SECS`, `HIVE_BROWSER_DB_GC_GRACE_SECS`,
`HIVE_BROWSER_DB_GC_MAX_REAP_FRACTION`); the browser-side glue
(`www/sqlite/hcb1.js`, `sync-client.js`, the worker's
`sync-state`/`sync-export`/`sync-apply`/`wipe` ops) that maps the capability
to worker calls and enforces the OPFS-side caps; the build-path resolution
of the block. Sync cadence is caller-driven at the glue level (sync-client.js
syncs when told; there is no server-pushed invalidation yet — a fleet-side
writer is only ever picked up on the next browser-initiated round); the
production run-node worker drives it on write, on a 30s periodic round, and
on demand (`dbSyncNow`) — see §5.
Fleet-fleet replica convergence flows through browser carriers (a Team
browser that synced node A's changes pushes them to node B's replica on its
next round — per-site watermarks make this exactly the same protocol); a
direct fleet↔fleet arm is a later row, not a correctness hole, because the
contract's system of record only ever converges THROUGH granted writers.

Beyond the exchange row: direct server-function database access (injected
DSN / host op), per-tenant aggregate fleet quota and billing treatment,
multiple named databases per project, richer dashboard management UI (a
single honest status line on `/run-node` landed with the production wiring,
§5), server-pushed
sync scheduling, and any cross-project sharing. None of these are needed to
implement the exchange, and each changes this contract when it lands.

## 10. The REST/Hrana surface (bn-browser-db-rest, landed)

Non-browser clients — a server, a CI job, `curl` — query the same replica
over plain HTTP, without embedding a QUIC client. This is a query path
ADDED to the contract, not a change to any sync rule above: the system of
record is unchanged, and every write here is just another granted writer
landing changes the CRR merge already knows how to carry.

- **Credential.** `POST /v1/projects/:project/browser-db/rest-token`
  (tenant-member JWT) mints one of two per-project bearers: team
  (read+write) and public (read-only, mintable only while
  `browser_db.public_read` is enabled and re-checked live on EVERY request,
  so disabling `public_read` kills an already-minted public token
  immediately). The plaintext is shown exactly once; only its SHA-256 hash
  is stored, in the replicated ProjectSettings row
  (`browser_db_rest_team` / `browser_db_rest_public`), checked
  constant-time. Minting again rotates.
- **Endpoints.** `libsql://api.<platform_domain>/v1/browser-db/<project>/`
  speaks Hrana v2/v3 pipelines (`hrana_proto::execute_pipeline` verbatim,
  so all wire type rules of the managed-SQLite lane hold identically) and
  `POST https://api.<platform_domain>/v1/browser-db/<project>/sql` is the
  Upstash-style `{query, params}` → `{fields, rows, rowCount}` shape. The
  libsql base URL's trailing slash is load-bearing (clients resolve
  `v2/pipeline` relatively). Both URLs are published server-derived from
  `api_base()` — in the mint response and in the `rest` block of
  `GET /v1/projects/:project/browser-db/status` — never reconstructed by
  a client.
- **CRR-safety invariants (the §-precedent "never point Hrana at a
  browser-dbs file" rule, refined).** The hazard was always a BARE SQLite
  connection: `sqlite_pool`'s extension-less `rusqlite::Connection` writes
  without maintaining the clock tables the merge reads. This surface opens
  via `hive_crsql::open` (extension loaded) and wraps every request in one
  explicit transaction with `hive_crsql::set_ts` applied right after BEGIN
  on any non-read-only request, so REST writes maintain the same clock
  tables a sync apply does. Read-only enforcement is a hard statement-class
  denylist (PRAGMA/ATTACH/DETACH, comment/whitespace-aware) plus refusal of
  the "sequence" request type — a per-connection `PRAGMA query_only` is not
  a boundary, because SQLite does not gate PRAGMA execution behind the flag
  it controls.
- **Ownership.** There is no stored `host_node`; every request proxies to a
  deterministic rendezvous-hash owner (`browser_db::rest_owner_for_project`
  — the `inference::coordinator_for` pattern, computed identically on every
  node from the gossiped healthy roster), which lazily opens-or-creates its
  local replica on first request (the reconcile loop's self-heal, triggered
  synchronously).
- **Caps.** `max_bytes` binds REST writes exactly as it binds sync applies:
  measured AFTER the RESERVED lock is held, and a real detected write past
  the cap rolls the whole transaction back with a typed quota message.
  `max_value_bytes` stays sync-boundary-only by design (an oversized value
  persists in its origin replica but never replicates).
- **v1 limits, stated honestly.** No interactive baton streams — every
  pipeline request is self-contained (`baton` in replies is always null; a
  presented non-empty baton gets an honest STREAM_EXPIRED); no v3 cursor
  (404 naming the gap); a client pipeline may not emit literal BEGIN/COMMIT
  SQL text (one transaction is already open — use Hrana's batch request
  type for atomic groups).
