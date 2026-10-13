# AGENTS.md

Present-tense conventions: what must be true of the code now. History and
incidents live in `CHANGELOG.md`, never here. **Self-maintenance:** stay under
~30 KB (`wc -c AGENTS.md`); past it, recompile from the code, the git log and
the memory files, keep only what is still true and load-bearing, and delete the
memories whose durable content was absorbed.

## Process & verification

- Every task runs through the `gm` skill (MCP `mcp__gm__gm`), subagents too:
  `codesearch`/codeinsight, never grep/find; git only via the `git_*` verbs
  (`git_commit paths:[...]` beside other sessions' edits), never raw git.
  Attribution is dywongcloud ONLY — no `Co-Authored-By`/trailers.
- **No test files, ever** (no `test/`, `__tests__/`, `spec/`, `#[cfg(test)]`,
  mocking libraries); verify with `cargo test --workspace` plus live execution.
  Fix a failure CLASS once at the shared primitive, never per call site.
  `cargo check` skips `cfg(target_os = "linux")` code AND test targets: run
  `cargo test --workspace --no-run`. A dependency change ships its `Cargo.lock`
  diff in the same pass (`--locked`).
- `scripts/build-fleet-binary.sh [2.38|2.39] [--clean-jemalloc]` builds fleet
  binaries (verifies `.symtab`); a link failing on undefined
  `sdallocx`/`mallocx` is a 96-byte stub `libjemalloc.a` from a killed build
  (`--clean-jemalloc`). A roll is a production change: operator's word, then
  `ansible-playbook ansible/playbooks/parallel-deploy.yml --tags backend`; prove
  the RUNNING binary with `strings -a /proc/<MainPID>/exe | grep -c '<log
  text>'`. `scripts/deploy-ui-fleet.sh` rolls the dashboard; it awk-matches
  `ansible_host=` INCLUDING comments — delete dead hosts, never comment them.

## Fleet, hosts, access

- Live roster = non-`hive_retired` lines of `ansible/inventory/hosts.ini`:
  fc-orlando 209.23.0.183, va, va3, sj (chain head), sj7, phx; va2/fr/sp/bkk/hk
  were DELETED but still named in `[glibc*]`/`[browser_nodes]` (undefined to
  ansible). Every live host is glibc 2.39 (`[glibc238]` has no live member;
  `scripts/audit-runtime-versions.sh`); membership is by OS image, never
  region. `[platform:vars]` templates `HIVE_CP_OWNER_CHAIN=fc-sanjose,
  fc-phoenix,fc-virginia-3,fc-virginia` and `HIVE_CP_VOTERS=fc-sanjose,fc-phoenix,
  fc-virginia-3`; a node drop-in overrides the unit — read `/proc/<pid>/environ`.
- SSH `ssh -i ~/.ssh/billing.pem root@<ip>` (orlando: `sysadmin`, `id_ed25519`).
  Verify ports from a FLEET vantage, never the laptop. TCP+UDP 20000-29999 is
  SG-open to the internet on every host — never run an ad-hoc file server
  (`scripts/audit-public-listeners.sh`). `scripts/hive-lockdown.sh` is the only
  host firewall; its PEERS roster is generated (`gen-hive-lockdown.sh`, then
  `site.yml --tags lockdown`), never typed.
- fc-orlando SHARES its host with a second hive-cloud (`autheo-devhub-1`,
  `hive-devhub.service`): stop by MainPID, never `pkill -x hive-cloud`; DevHub
  is un-versioned, never "fixed" here. macOS launchd nodes:
  shadw1 is the SYSTEM LaunchDaemon `system/dev.shadw.shadw1` (a gui/501 job
  with that label is a crash-looping duplicate); `kickstart -k` keeps the OLD
  env, so a plist env edit needs bootout → SIGTERM → bootstrap; recall `macOS
  launchd nodes`. The Tencent account is SHARED: attribute via CloudAudit before
  deleting; GAAP proxies `zzhli-*`/`kjh-*` are other tenants'.
- Seed `HIVE_BOOTSTRAP_PEERS` with ADDRESSED peers
  (`<64hex>[@ip:port[+ip:port]][|relay-url]`), never bare ids; `peer_iroh.json`
  cannot seed another node. `HIVE_TRUSTED_NODE_IDS` + `HIVE_PEER_TRUST=1` list
  every fleet id on every node (missing: `REJECTED mutating gossip: no
  verified+trusted signer`); join rides `STREAM_JOIN` (QUIC remote id + an HMAC
  over `HIVE_JWT_SECRET`). Admin JWTs are HS256 on `HIVE_JWT_SECRET`, minted ON
  the node (`scripts/mint-operator-jwt.sh`; `platform_admin` needs an email in
  `HIVE_OWNER_EMAIL`/`HIVE_ADMIN_EMAILS`).

## Control plane & leader-only jobs

- With `HIVE_CP_OWNER_CHAIN` set the owner is the first chain entry PRESENT in
  `registry.nodes()` with a `peer_id` and public address
  (`Cluster::strict_chain_owner`) — never the observer's `healthy` flag, never
  the identity election; no qualifying entry = HOLD. Every proxy and
  owner-forward asks `CloudState::leader_forward_target()`.
- **Every leader-only loop asks one question, `leadership::may_act(cloud, Job)`**:
  owner AND fresh view AND not isolated AND (backup owner only) fresh DIRECT
  contact with a strict majority of `HIVE_CP_VOTERS`, held for
  `Job::min_tenure()` (NodeDeathRelocate 600 s, Acme/AcmeHttp01 300,
  BillingMeter 120, DnsWrite/GitPoll 60, others 30); take-over keys on the
  ownership TERM (`TERM_LAPSE`, 30 s). An owner present but unable to act is
  still owner everywhere, so every job HOLDS fleet-wide — restore
  transport/voters, never weaken the gate. Node-death self-heal is OFF unless
  `HIVE_NODE_DEATH_SELF_HEAL=1`.
- **Automated incidents are level-triggered** (`incidents.rs`): openers call
  `IncidentStore::open` EVERY pass with a stable `condition` + `ttl_ms` ≈ 3
  passes and `clear(condition)` when healthy; `Job::IncidentReconcile` expires
  and bounds (`HIVE_INCIDENT_HISTORY_MAX` 500, `HIVE_INCIDENT_RETENTION_SECS`
  7 d).

## State: replication, routing, relational mirror

- Canonical state is ONE in-memory representation (`state.json` + store_sync),
  never derived from, translated through or gated on GuardianDB or its SQL
  mirror (at most a read-only projection).
- `admin_ingress` forwards MUTATIONS to the leader and serves GET/HEAD LOCALLY;
  public hosts are round-robin DNS, so a GET over NODE-LOCAL data (a file under
  `persist::data_dir()`, an in-process `Mutex`/`OnceLock`) answers empty/404
  from the wrong node. Such a GET needs an owner/leader proxy (`build_get`,
  `fetch_from_host`, `fetch_bytes_from_host`; the owner re-checks the bearer
  and never re-proxies) plus a `gossip::dispatch` arm (longest-prefix-first),
  or the state moves into `store_sync::REGISTRY` or fans out
  (`db_replicate::fanout_all` + `apply_mirrored_write`).
- **Store-sync adoption is WHOLESALE-REPLACE, so only from an owner every node
  agrees on** (`OwnerSource::may_adopt_wholesale_from`: the chain HEAD, or the
  identity election on a chain-less mesh); from a backup owner or `Fallback`
  only per-row `MERGE_STORES` merge. A critical write lands on EVERY owner-chain
  candidate; snapshots route through `serde_json::Value` + `enc_sorted()`.
  ProjectSettings rows replicate with per-row `updated_ms` + permanent
  tombstones; absence never erases a row.
- `ProjectSettings` holds `last_build` (terminal only, error ≤ 400 B),
  `last_build_id`/`last_build_started_ms` (an admission pointer — "building" is
  derived from the live BuildStore, never stored) and grow-only
  `first_deployed_ms`. The ONLY writer of a build's outcome is
  `BuildCompletionGuard::drop` (every build-task exit crosses it); boot replays
  terminal records (`persist.rs`).
- **The relational index is built by ONE background walker, never on a request
  path, and nothing writes the catalog before it completes once.**
  `spawn_index_refresher` walks under `HIVE_RELATIONAL_INDEX_BUILD_SECS` (900),
  re-walks every `HIVE_RELATIONAL_REFRESH_SECS` (120); `index_ready()` gates
  `init_schema` and `ensure_table_exists` (verified via
  `information_schema.tables`, never `SELECT 1 FROM t`). Cron is NODE-LOCAL
  (restore via `CronScheduler::replace_all`, never `add()`).

## Mesh transport, discovery, watchdogs

- `PeerPool` keys trunks by ENDPOINT ID, never the caller's label. **Every
  inbound accept is bounded, budgeted by its NEGOTIATED ALPN, and never evicts
  an established trunk**: `HIVE_P2P_ACCEPT_DEADLINE_MS` (40 s; past it the
  connection closes `FLEET_CLOSE_OVERLOADED`, counted `accept_stuck`);
  pre-handshake the ClientHello picks the budget (browser 128 / fleet 512 /
  unparseable → `pending`, `HIVE_P2P_PENDING_MAX_CONNS` 256); every hybrid
  X25519MLKEM768 dial lands in `pending`. `hive_p2p::establish_stats` is served
  on operator-only `GET /v1/mesh/establish`, never the public `/v1/mesh`.
- `vendor/iroh` keeps two patches across pin bumps: relay "read before send" and
  `PendingRebind` (a failed rebind is RETRIED; `AddrInUse` on the pinned
  `HIVE_IROH_PORT` otherwise leaves UDP closed). A `hive_p2p::dht` failure
  leaves it UNREGISTERED with a WARN, never failing `bind()`; DHT records are
  public forever, default relay-only (`HIVE_DHT_PUBLISH_DIRECT=1`).
- **A gossip round never waits on a dead peer, and one stale reading never
  withdraws one.** Rounds end at `HIVE_GOSSIP_ROUND_DEADLINE_MS` (8000) through
  ONE primitive, `bounded_round::BoundedRound`; a target that missed the deadline
  `LEASH_AFTER_ROUNDS` (2) times running is waited on only
  `HIVE_GOSSIP_STRAGGLER_LEASH_MS` (2500); a leash PARKS, never cancels, keyed
  by ENDPOINT id. `health::demote` withdraws only after
  `DEMOTE_STALE_ROUNDS` (2) and HOLDS while stalled.
- meshwatch: total isolation (600 s), cumulative degradation, and
  `establishment_wedge` — transport counters (`HIVE_MESH_ESTABLISH_WEDGE_SECS`
  300), never peer counts. **Every automatic restart passes
  `ControlledRestart::request` → `RestartReason::admissible`**: refused until
  `hive_backend::orphan_reap_ran()`, rate-capped per reason (wedge 1/6 h) from
  `restart_history.json`, failing CLOSED when unreadable.

## Containers, podman, process lifecycle

- **`KillMode=process` on hive-node.service is load-bearing** (rootful podman's
  conmon inherits the caller's cgroup; the default KillMode SIGTERMs every
  managed database per roll); `OOMPolicy=continue` likewise; `TimeoutStopSec=90s`
  is explicit. Tenant CELLS therefore outlive their process, so every boot reaps
  its own previous boot's cells (`hive_backend::reap_orphaned_cells`). Ownership
  is owner + boot (`hive.owner`, `hive.boot`), never boot alone; another owner's
  cell is never touched; removal is always `rm -f -v`. `reap_orphaned_runners`
  strips the kernel's ` (deleted)` suffix (`strip_deleted_exe_suffix`); every
  `/proc/<pid>/exe` compare needs the same.
- Graceful stop finishes inside systemd's timeout: a `persist()` after
  `flush_blocking` closed admission is REFUSED, never blocked;
  `HIVE_SHUTDOWN_DEADLINE_SECS` (75) runs on a std thread. Every container
  removal passes `-v` (`container_cli::rm_args`): podman takes one lock from a
  FIXED pool (2048) per container AND per volume; leaks surface as 503
  `CAPACITY_EXHAUSTED`. **Never `podman volume prune`** — reclaim is gated on
  `is_anonymous_volume` (64 ascii-hex) AND `dangling=true`.

## Isolation backends & Litebox

- **Capability is PROBED and ADVERTISED, never assumed**, on the filesystem the
  cell execs against: `NodeInfo::wasm_runtime` / `bun_runtime` come from
  `resources::RuntimeCapabilitySource`; `None` = NOT CAPABLE (unlike
  `disk_free_gb == 0`). Placement gates through ONE predicate
  shared by `place`, lease-stickiness and `dispatch_fallbacks`
  (`schedule::wasm_capable`, `bun_capable`, `build_isolation_capable`); a
  missing runtime is a NODE fault (`fault::NODE_RUNTIME_MISSING`).
- Litebox (third `CellBackend`, Firecracker → Litebox → Mock): a node is Litebox
  only after `hive-cloud --litebox-probe` PASSES on that host with the exact
  staged runner AND `litebox_verified=true` on its `hosts.ini` line — never the
  flag alone, never macOS. Guest FS: uid 1000, read-only tar lower layer over an
  in-memory upper; tar headers are ustar ONLY (`Header::new_ustar`; GNU
  longnames silently truncate the guest fs); the runtime binary is staged INTO
  the guest tar. Guest fork is sound only if the child `execve()`s at once
  (pipelines and subshells abort the guest).
- `litebox-bind-shim.js` (`realpath.native`, `os.networkInterfaces`,
  `process.title` as a JS-only accessor — a native assignment zeroes
  `process.env`) and the guest `/etc` files are inputs to
  `runtime_source_sha256`, so ANY change rebuilds every runtime artifact. Guest
  egress is TCP-only through the per-cell TUN (`/etc/resolv.conf` + `options
  use-vc`, `NODE_EXTRA_CA_CERTS` at a real bundle, denies inserted `-I FORWARD 1`,
  admin port denied; TCP_KEEPIDLE/KEEPCNT accepted-and-ignored so undici's
  single-address `fetch` works). `sandboxes_platform::supervise_exec` drains
  each exec stream in its own task (`HIVE_SANDBOX_RUN_MS` 110 s /
  `HIVE_SANDBOX_EXEC_MAX_MS` 1 h, `killpg` on expiry); the guest tar and its
  `/proc/self/fd` alias are Drop guards of the WAITER task. PVM kernels:
  `pti=off` is REQUIRED and `KVM_CREATE_VM` succeeding proves nothing; recall
  `PVM kernels on the fleet`.

## Runtime discipline (the serving runtime never stalls)

- **Heavy work never runs on a serving tokio worker.** The guardian SQL engine
  reloads every row of each table per STATEMENT, so `relational::exec` is the one
  chokepoint and runs under `block_in_place`; the mirror loop, index walker,
  replication writer and admin mirror reads live on the `bulkhead` runtime
  (`bulkhead::spawn`, `HIVE_BULKHEAD_THREADS` 2). `runtime_watch` probes every
  1 s from an OS thread and names the CPU-hot threads of a stall >2 s
  (`/v1/admin/runtime`).
- Anything a request path reserves is released by a `Drop` guard, never only on
  the `Err` branch (axum drops the future when the client leaves):
  `ColdStartGuard`, `PooledConn`, `InflightSlot`, litebox guest tars, sandbox
  drains. Count failure streaks at the ONE chokepoint every caller funnels through.

## Builds & deploys

- `git push` auto-deploys through TWO triggers deduping on the commit SHA
  (`CloudState::git_poll_seen`): the webhook and `git::spawn_git_poll_reconcile`.
  **A single unreachable node never fails a deploy**: on
  `FanoutOutcome::nothing_ran()` `run_build` walks `schedule::dispatch_fallbacks`
  one candidate at a time with `place`'s `capable` + `reachable` predicates,
  bounded by `HIVE_DEPLOY_DISPATCH_FALLBACK_MAX` (3). `build interrupted: node
  restarted…` (`git.rs:141`) means the HOLDER restarted — read the TARGET's store.
- Isolated BuildExecutor: recover `HIVE_BUILDAH_SOCKET` from the tenant
  process's `/proc/<pid>/environ`; never flip `build_isolation_protocol`. The
  sealed-artifact writer opens every path component O_NOFOLLOW, so a macOS node
  whose HIVE_DATA sits under `/var/folders` fails every build ("open no-follow
  runtime artifact scratch component") — use `/private/tmp`.
- Checkouts live in `git::deploy_root()` (`$HIVE_DATA/deploys/<project>-<ms>-
  dpl-<BUILD id>`); prefix scanners cover BOTH roots and BOTH name forms via
  `git::checkout_prefixes`. A missing direct entry fails the BUILD
  (`git::preflight_direct_entries`), never the launch, never as a node fault; a
  `fluid.json` lane runs ZERO install/build commands.
- `project_inventory.rs` serves `GET /v1/gitops/projects?view=summary`: tenant
  rows by `record_tenant` (never `norm` on a stored tag), one pass over the
  gateway + `peer_deployments`, no relational tier, `require_auth_read`, leader
  read-through with the polled 3 s budget then local, gossip arm
  `/v1/gitops/projects`; the default view stays byte-identical (GitOps commits
  and the CLI depend on it). A same-tenant re-import of a never-deployed name
  with the same repository is a retry (`name_reuse`: resets root_dir/branch/
  inference only); ownership is a CAS (`claim_team_exact`); an in-flight build
  answers 409; `HIVE_MAX_EMPTY_PROJECTS` (25) caps never-deployed rows;
  `project_redeploy` is the only path that rebuilds from `last_build`. The
  dashboard's project lists come from `ui/lib/projects.ts` (`useProjects`),
  never from `/deployments` alone.
- Env: legacy scope `""` matches build AND runtime (`env_scope_matches`);
  build-inlined values (`NEXT_PUBLIC_*`) need `$HIVE_DATA/build-cache/*.tar`
  cleared before a redeploy of unchanged source — verify the SERVED bundle.
- **The host build lane's package registry is part of node health.** The host
  lane inherits `/root/.npmrc` (Tencent images: `mirrors.tencentyun.com/npm`), so
  a dead mirror resolver fails every uncached install behind a warm cache.
  `build_registry::assess` (host lane only) probes once per build and, only for
  a dark KNOWN public mirror (exact hosts; `HIVE_BUILD_REGISTRY_MIRRORS` extends)
  while registry.npmjs.org answers, hands the build `npm_config_userconfig` = a
  0600 copy of the host `~/.npmrc` with the registry replaced by the public one,
  atomic under `persist::data_dir()` — a repository's own
  `.npmrc`/`.yarnrc`/`pnpm-workspace.yaml` still wins by npm's config layering.
  Verified npm 10/11, pnpm 9-12 (pnpm 11+ ignores `npm_config_registry`; the
  fleet pins 12.6.0), yarn 1; bun ignores userconfig. A lockfile whose
  `resolved` URLs name the dead mirror still fails `npm ci`.
  `HIVE_BUILD_REGISTRY_GUARD=0` disables; `scripts/audit-build-registry.sh` is
  the fleet check. Never hand-edit a node's `/etc/resolv.conf`: reproduce DNS
  faults in `unshare -m` + a bind mount; `nmcli general reload dns-rc` restores
  the file.
- `fluid_build::framework::generic_node_server_entry` is the generic-Node rule:
  `server.{cjs,js,mjs,mts,ts,cts}` at the root or `src/`, only when the file
  contains `listen(`/`createServer(`/`serve(` and the repo shows no static intent
  (no root `index.html`, no `vercel.json` outputDirectory);
  `discover_worker_entry` requires a wrangler config; TypeScript entries run with
  `--experimental-transform-types`; the express launcher polls `$PORT` to
  `HIVE_LAUNCHER_LISTEN_DEADLINE_MS` (60 s) and never double-listens.

## Placement & capacity

- Disk is a HARD filter (`HIVE_PLACEMENT_DISK_FLOOR_GB`), never a score term;
  `NodeInfo::disk_free_gb` is gossiped on a timer and unknown (`0`) ADMITS;
  placement requires an actually dialable path. Data images carry a DOUBLE
  prefix (`dpl-dpl-<hash>.data.ext4`), so a keep-set of raw deployment ids
  matches nothing; `gc_rootfs_images` matches both forms and refuses an empty
  keep-set or an orphaned fraction over `HIVE_GC_MAX_REAP_FRACTION`. **Every
  reclaim path needs the same blast-radius guard**: `browser_artifacts::gc`,
  `browser_db` GC, the guardian reaper (refuses when `HIVE_NODE_ROSTER` is unset).

## Request path, circuits, cold start

- A deployment fails in two shapes: starts then dies (`crash_streak` +
  `last_warm_ok_ms`) or never listens (`warm_fail_streak`); never clear a streak
  because a start succeeded. A broken deployment reports
  `DEPLOYMENT_CIRCUIT_OPEN`, never `CAPACITY_EXHAUSTED`; the circuit is keyed on
  instance HEALTH (`unhealthy_reaps`), never instance count;
  `CIRCUIT_PROBE_INTERVAL_MS` (30 s) outlasts the failure it guards.
  fluid-tunnel: `LOCAL_CONNECT_DEADLINE` (15 s)
  and a 60 s response deadline bound every exchange; `UNHEALTHY_FAILURE_STREAK`
  (5) closes the tunnel; bodiless responses (HEAD/1xx/204/304) end the exchange.
- Cold start: `acme::SniResolver::resolve` hands the SNI to `prewarm`
  (`HIVE_PREWARM=0` off); `fluid_compute::Fluid::warm` is the ONE warm primitive,
  shared with keep-warm. Litebox hashes each app archive ONCE per boot
  (`verified_app_archive`) and drops the node-global `artifact_lock` right after
  `spawn` — never hold it across `wait_litebox_ready`. Tenant tier lives in BOTH
  `c.teams` and `c.billing`: every tier change goes through
  `admin::apply_plan_everywhere`.

## Managed data lanes

- Managed SQLite (`DbKind::Sqlite`) and `browser_db` share nothing but the word:
  a file per DATABASE at `$HIVE_DATA/sqlite-dbs/hive-sqlite-{sanitize_tag(db_id)}
  .db` (musql engine: same stem + `.musq`) vs a cr-sqlite CRR per PROJECT at
  `$HIVE_DATA/browser-dbs/`. **Never point the Hrana handler or musql at a
  `browser-dbs` file** — a bare `rusqlite` writer (`sqlite_pool`) bypasses the
  clock tables the CRR merge reads; `browser_db_rest` is CRR-safe only because
  it opens via `hive_crsql::open` and calls `set_ts`. The SQLite lane is
  owner-routed, never leader-routed; recall `Managed SQLite (Hrana) wire
  contract`.
- `browser_db`: grants ride the admission, server-derived and tenant-pinned;
  caps bind BOTH sides (`max_bytes` 64 MiB, `max_value_bytes` 1 MiB; over-cap =
  typed refusal + whole-batch rollback, never truncate/evict).
  Bytes/site-ids/watermarks replicate ONLY through `hive_crsql` ChangeBatch
  (`Op::CrrSync`); the fleet re-checks the grant on EVERY request, deriving
  tenant+project+file server-side (`db_file` is an identifier, never a path).

## Browser lane

- `fluid_core::browser_policy_digest` and `policyDigest` in
  `crates/hive-browser/www/artifact-policy.js` are ONE contract with TWO
  implementations. A browser function is eligible by fluid.json opt-in (FAILS
  THE BUILD when ineligible) or build-time auto-detection; ESM is REWRITTEN to
  CommonJS (`browser_esm.rs`). Bytes stay node-local
  (`$HIVE_DATA/browser-artifacts/<policy_digest>.js`); only descriptor metadata
  replicates; delivery re-verifies size and source BLAKE3 (404 for a foreign
  tenant); admission capabilities are server-derived — `AdmissionRequest.digest`
  is never read, `trusted_callers` comes from the live HEALTHY registry.

## DNS: Seer, cutovers/ACME, custom domains

- Records are the scarce resource, queries are not: Seer answers the
  fleet-served deploy zone affinity-first from the route table
  (`apps_host_owner`). Vercel DNS has no geo/health routing, so Seer answers
  only for names delegated to it and `desired_geo_delegation` refuses to
  publish below TWO nameservers. **A delegated name must never serve NEITHER
  addresses NOR delegation**: Vercel refuses NS creation while any child record
  exists, so a cutover is a restore-on-failure transaction in
  `vercel_dns::plan_writes` (creates before deletes, VERIFIED rollbacks) and
  `ReconcileGuards`' create circuit skips delete-first steps while creates are
  broken.
- **Advertise only what peers have PROVEN**: `NodeInfo::dns_ns` is necessary,
  never sufficient; `dns_probe` proves reachability and `validate_nameservers`
  admits a node attested from TWO distinct REGIONS, never self-attested. Below
  two proven nameservers the reconciler HOLDS the published set and opens an
  incident; `publishable()` is damped both ways; DNS-01 is gated on every
  authoritative nameserver.
- Custom domains: routing keys are FULL hostnames (`fluid_gateway`'s
  `aliases_full`) before any first-label or wildcard lookup. Attachment needs
  observed DNS proof — the activating node itself observes `_hive-verify.<domain>`
  via DoH — through ONE helper (`mark_domain_verified`); only a VERIFIED record
  locks to its tenant; detach and activate require `require_domain_owner_if_exists`
  AND a stored challenge whose project matches the path. HTTP-01 answers from
  the replicated `Http01Store` (a miss proxies to the leader). Reserved proof
  names (`_acme-challenge`, `_hive-verify`, any case) are rejected in
  add/import/zone-parse.

## Security gates (never weaken)

- **A tenant string never becomes a host path.** Podman reads `-v <a>:<b>` as a
  bind mount whenever `<a>` looks like a path, so the left side is always a
  platform-templated NAME (`container_volume_cfg` → `hive-vol-{sanitize_tag(
  project)}`); same for DB file names, browser artifact digests (64 lowercase
  hex, else 400) and snapshot dirs.
- **Tenancy comes from the cryptographically-bound JWT claim, never the spoofable
  `x-hive-team` header**; an untagged tenant is a fail-closed sentinel
  (`UNTAGGED_TENANT`), never collapsed to "personal". Gossip arms carrying a team
  token parse it with `team_claims()`/`qparam()`, never a raw `split_once`.
- **The auth middleware answers an anonymous GET/HEAD as `ANON_TENANT`**
  (`__anon__`, `x-hive-anon: 1`); anonymous scoping is PER-HANDLER, so a
  handler that ignores it is world-readable. Gate a public route with a separate
  wrapper (`admin::require_operator`, `require_internal_hop`, `auth::enforced()`;
  `fleet_deployments_public` beside the mesh-shared `fleet_deployments`), never
  inside a handler `gossip::dispatch` also calls.
- `HIVE_SECRET_KEY` is the fleet-shared at-rest key: never introduce or rotate it
  without the previous key in `HIVE_SECRET_KEY_OLD` (comma-separated hex,
  `secrets.rs:74`) — `decrypt` returns its input unchanged on AEAD failure;
  `try_decrypt` is the honest `Option` variant. Never print a plist, a manifest
  or `/proc/*/environ` wholesale — they carry this key. `ProjectStore::put_env`
  force-masks credential-shaped values (`project_settings::looks_like_secret`)
  regardless of the UI flag.
- PQ transport: the mesh QUIC transport offers hybrid X25519MLKEM768 first via an
  explicit `.crypto_provider(...)` in `hive_p2p::bind_full` (iroh's presets
  prefer plain `ring`); report the REAL negotiated group (`pq_kex_stats`).
  **Transport IDENTITY stays classical ed25519.** TLS resumption is fleet-shared
  (`FleetTicketer` in `acme.rs`: key derived from `HIVE_SECRET_KEY`, 6 h
  rotation, current-or-previous epoch, 0-RTT off, `HIVE_TLS_SHARED_TICKETS=0`
  disables).

## Hashing: content identity vs. security binding

**BLAKE3 is the platform content-addressing hash.** `fluid_core::content_hash` /
`ContentHasher` mint every NEW content identity; `verify_content` accepts a
stored digest under EITHER BLAKE3 or the legacy hex(SHA-256) over the same
domain. Digests minted before domain separation (transfer chunks, runtime-artifact
package/semantic-tree, litebox archives) are a bare hash of the bytes and use
the explicit pair `raw_content_hash` / `legacy_raw_content_sha256` /
`verify_raw_content` — never cross a domain-separated site with a raw one.
`content_verify_stats()` (`GET /v1/admin/content-hash`) counts which algorithm
each accept used. hex(BLAKE3) and hex(SHA-256) are both 64 lowercase hex:
`validate_sha256`/`validate_digest`/`is_hex64` prove shape, never algorithm;
`verify_content` is the ONLY thing that decides which algorithm a stored digest
came from. **A digest that NAMES stored bytes** (a store directory, a
content-addressed filename, a GC keep-set entry) **keeps its writer until every
node reports `accepted_sha256` flat across a full deploy cycle** — never a flag
day. A LOOKUP KEY (`hive-build-cache-v4`) may move.

### Exclusion list — SECURITY BINDINGS, never touch the algorithm

- **Signatures (ed25519).** `hive-cloud/src/integrity_signer.rs:19,24,47,60,80`
  (per-node keypair) · `hive-core/src/integrity.rs:32,161,168,180` — this
  SHA-256 chain fold LOOKS like content addressing and is not: its output is the
  signed message, so changing it invalidates every signature ever made ·
  `hive-p2p/src/lib.rs:319,328,379,729` · `hive-cloud/src/gossip.rs:2191` ·
  `hive-cloud/src/browser_admission.rs:785,1034,1826` ·
  `hive-browser/src/lib.rs:1174,1331,1340,1359,1372,1387,2503,2565` ·
  `hive-p2p/src/dht.rs:33` · `hive-cloud/src/discovery.rs:16` ·
  `hive-cloud/src/admin.rs:9547` (classical ed25519; no ML-DSA).
- **MAC / HMAC.** `hive-cloud/src/admin.rs:8866,8890,8896` (`hmac_sha256`,
  GitHub + Stripe webhooks) · `hive-cloud/src/main.rs:1712,4139` (mesh hot-join
  HMAC) · `hive-cloud/src/git.rs:10891,10918,10927,10946` · `webhooks.rs:15,204` ·
  `billing.rs:1867` · `db_rest.rs:1374` · `tencent_eip.rs:28,305-338` (TC3) ·
  `hive-p2p/src/lib.rs:329,3858`.
- **TLS / certificates.** `hive-cloud/src/acme.rs:294-311` (`FleetTicketer` key
  derivation) · `acme.rs:1277-1287` (`custom_bundle_name`: SHA-256 of the domain
  IS the bundle's on-disk name — never rehash).
- **Credential hashing.** `apikeys.rs:10,13` · `browser_db_rest.rs:138-141,209,256` ·
  `drive_api.rs:485-489,529,588,646,673` · `world.rs:1096-1101,1054` ·
  `enterprise.rs:281-324` (Argon2id current; the legacy `sha256(project ":"
  password)` verify branch must keep working) · `zkauth.rs:44,265` and
  `hive-zkauth/src/lib.rs:34,231,243,260` (SHA-512).
- **Handshake / supply-chain proofs.** `hive-backend/src/firecracker.rs:3471`
  and `hive-cell-agent/src/main.rs:336` (`agent_handshake_transcript`) ·
  `hive-cloud/src/build_executor.rs:4290` (`runsc.sha256` against a published
  value) and `:4308` (`image_digest` parses a literal OCI `sha256:`).

### Content identity — already BLAKE3, do not move

`fluid-core/src/lib.rs:837-838` (`browser_source_digest`), `:851-905`
(`browser_policy_digest`, mirrored in `hive-browser/www/artifact-policy.js`) ·
`hive-browser-proto/src/lib.rs:133` (`BLAKE3_DIGEST_LEN = 64`), `:316` ·
`hive-cloud/src/browser_artifacts.rs:450-462,508-513,969-1000` (policy digest is
the store filename, the route and the ETag) ·
`hive-cloud/src/browser_db.rs:1694,1719,1720-1732,1864,2003,2076` ·
`hive-cloud/src/guardian.rs:3396` (`iroh_blobs::Hash` is BLAKE3) ·
`hive-cloud/src/git.rs:10790-10834` (`compute_cache_key`).

### Content identity — still SHA-256, writer deliberately not flipped

`hive-backend/src/runtime_artifact.rs:142,239-243,302,315,1105,1621,2151`
(`content_sha256` IS the store directory name and binds `semantic_tree_sha256`;
a rehash orphans every staged artifact) · `hive-cloud/src/deployment_ledger.rs:772`
writer / `:786,831` reader (dual-accept over the bytes AS STORED) ·
`fluid-core/src/lib.rs:3152` (`normalized_manifest_sha256`, consumed by
`git.rs:5746`, `runtime_artifact_transfer_wire.rs:502,529`,
`runtime_artifact_transfer_sender.rs:111`) ·
`fluid-build/src/deployment_snapshot.rs:305`, `repository_snapshot.rs:303` ·
`hive-cloud/src/repository_build.rs:196` · `app_discovery.rs:128` ·
`build_executor.rs:889` · `billing.rs:690-715` (ledger checkpoint chain).
Dual-accepting readers: `deployment_ledger.rs:786` ·
`runtime_artifact_transfer_wire.rs:456,529` ·
`runtime_artifact_transfer_sender.rs:111` ·
`runtime_artifact_transfer_service.rs:680` (chunk re-verify against durable
bytes) · `git.rs:10967` (`verify_pulled_artifact`: a peer on the legacy mint and
one minting BLAKE3 address the same bytes).

@.gm/next-step.md
