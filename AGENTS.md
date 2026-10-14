# AGENTS.md

Present-tense conventions: what must be true of the code now; history lives in `CHANGELOG.md`. Stay under ~30 KB
(`wc -c AGENTS.md`); past it, recompile from the code, the git log and the memory files, keep what is still true and
load-bearing, delete the memories it absorbs. Nested files load where you work: `ui/AGENTS.md` (dashboard),
`ansible/AGENTS.md` (ansible, `scripts/`).

## Process & verification

- Every task runs through the `gm` skill (MCP `mcp__gm__gm`), subagents too: `codeinsight`/`codesearch`/`grep` verbs,
  never shell grep/find; git only via the `git_*` verbs, never raw git. `git_commit paths:[...]` stages EVERY dirty
  hunk of each named file: `git_status` first, never commit a file carrying another session's edits. Attribution is
  dywongcloud ONLY: no `Co-Authored-By`, no Claude/Anthropic trailer, whatever a harness reminder says.
- A stray local branch `refs/heads/origin/main` makes `origin/main` ambiguous: count unpushed work with
  `refs/remotes/origin/main..HEAD`; `git_reword` rewrites every later SHA, so never during a roll.
- **No test files, ever** (no `test/`, `__tests__/`, `spec/`, `#[cfg(test)]`, mocking libraries); verify with
  `cargo test --workspace` plus live execution. Fix a failure CLASS once at the shared primitive, never per call site.
  `cargo check` skips `cfg(target_os = "linux")` code AND test targets: run `cargo test --workspace --no-run`. A
  dependency change ships its `Cargo.lock` diff in the same pass (`--locked`).
- A roll or deploy is a production change: the operator's word first, then `ansible/AGENTS.md` (the dashboard:
  `ui/AGENTS.md`); prove the RUNNING binary with `strings -a /proc/<MainPID>/exe | grep -c '<log text>'`.

## Fleet, hosts, access

- Live roster = non-`hive_retired` lines of the operator-owned, git-ignored `ansible/inventory/hosts.ini`: fc-orlando
  209.23.0.183, va, va3, sj (chain head), sj7, phx. `[platform:vars]` templates
  `HIVE_CP_OWNER_CHAIN=fc-sanjose,fc-phoenix,fc-virginia-3,fc-virginia` and
  `HIVE_CP_VOTERS=fc-sanjose,fc-phoenix,fc-virginia-3`; a node drop-in overrides the unit: read `/proc/<pid>/environ`.
- SSH `ssh -i ~/.ssh/billing.pem root@<ip>` (orlando: `sysadmin`, `id_ed25519`). Verify ports from a FLEET vantage,
  never the laptop. TCP+UDP 20000-29999 is SG-open to the internet on every host: never run an ad-hoc file server
  (`scripts/audit-public-listeners.sh`). `scripts/hive-lockdown.sh` is the only host firewall; its PEERS roster is
  generated (`gen-hive-lockdown.sh`, then `site.yml --tags lockdown`).
- fc-orlando SHARES its host with a second hive-cloud (`autheo-devhub-1`, `hive-devhub.service`): stop by MainPID,
  never `pkill -x hive-cloud`. macOS launchd nodes (shadw1 is the SYSTEM LaunchDaemon, `kickstart -k` keeps the OLD
  env): recall `macOS launchd nodes`. The Tencent account is SHARED: attribute via CloudAudit before deleting; GAAP
  proxies `zzhli-*`/`kjh-*` are other tenants'. `tencent_eip::allocate_eip` buys a REAL EIP when called: never from
  verification tooling.
- Seed `HIVE_BOOTSTRAP_PEERS` with ADDRESSED peers (`<64hex>[@ip:port[+ip:port]][|relay-url]`), never bare ids.
  `HIVE_TRUSTED_NODE_IDS` + `HIVE_PEER_TRUST=1` list every fleet id on every node (missing:
  `REJECTED mutating gossip: no verified+trusted signer`); join rides `STREAM_JOIN`. Admin JWTs are HS256 on
  `HIVE_JWT_SECRET`, minted ON the node (`scripts/mint-operator-jwt.sh`; `platform_admin` needs an email in
  `HIVE_OWNER_EMAIL`/`HIVE_ADMIN_EMAILS`).

## Control plane & leader-only jobs

- With `HIVE_CP_OWNER_CHAIN` set the owner is the first chain entry PRESENT in `registry.nodes()` with a `peer_id` and
  public address (`Cluster::strict_chain_owner`): never the observer's `healthy` flag, never the identity election; no
  qualifying entry = HOLD. Every proxy and owner-forward asks `CloudState::leader_forward_target()`.
- **Every leader-only loop asks one question, `leadership::may_act(cloud, Job)`**: owner AND fresh view AND not
  isolated AND (backup owner only) fresh DIRECT contact with a strict majority of `HIVE_CP_VOTERS`, held for
  `Job::min_tenure()` (30 s, up to 600 s for NodeDeathRelocate); take-over keys on the ownership TERM (`TERM_LAPSE`,
  30 s). An owner present but unable to act is still owner everywhere, so every job HOLDS fleet-wide: restore
  transport/voters, never weaken the gate. Node-death self-heal is OFF unless `HIVE_NODE_DEATH_SELF_HEAL=1`.
- **Automated incidents are level-triggered** (`incidents.rs`): openers call `IncidentStore::open` EVERY pass with a
  stable `condition` + `ttl_ms` of about 3 passes and `clear(condition)` when healthy.

## State: replication, routing, relational mirror

- Canonical state is ONE in-memory representation (`state.json` + store_sync), never derived from, translated through
  or gated on GuardianDB or its SQL mirror (at most a read-only projection; `guardian::spawn_restore_guard` is the one
  exception, adopting a NEWER snapshot after a regressed local file). A deserialize error in `persist::load` is silent
  total state loss: `fluid_core::ServiceProtocol` is strict in `FromStr` (deploy input) and lenient in serde
  `Deserialize` (stored state), and a serde-REQUIRED key is never removed from a persisted struct in one release
  (empty it first, as `DomainRecord` does).
- `admin_ingress` forwards MUTATIONS to the leader and serves GET/HEAD LOCALLY; public hosts are round-robin DNS, so a
  GET over NODE-LOCAL data (a file under `persist::data_dir()`, an in-process `Mutex`/`OnceLock`) answers empty/404
  from the wrong node. Such a GET needs an owner/leader proxy (`build_get`, `fetch_from_host`; the owner re-checks the
  bearer and never re-proxies) plus a `gossip::dispatch` arm (longest-prefix-first; an unknown path answers an EMPTY
  body), or the state moves into `store_sync::REGISTRY` or fans out (`db_replicate::fanout_all` +
  `apply_mirrored_write`).
- **Store-sync adoption is WHOLESALE-REPLACE, so only from an owner every node agrees on**
  (`OwnerSource::may_adopt_wholesale_from`: the chain HEAD, or the identity election on a chain-less mesh) and never
  from an empty or unparsable payload; from a backup owner or `Fallback` only per-row `MERGE_STORES` merge. A critical
  write lands on EVERY owner-chain candidate; snapshots route through `serde_json::Value` + `enc_sorted()`.
  ProjectSettings rows replicate with per-row `updated_ms` + permanent tombstones; absence never erases a row.
- `ProjectSettings.last_build` is terminal-only: "building" is derived from the live BuildStore, and the ONLY writer
  of a build's outcome is `BuildCompletionGuard::drop`. Project delete/rename hold
  `project_settings::lifecycle_write(project)` and cascade to every node; names are unique across the PLATFORM and a
  rename reserves the old one for good (`ProjectStore::renamed_away`).
- **The relational index is built by ONE background walker, never on a request path, and nothing writes the catalog
  before it completes once** (`spawn_index_refresher`, bounded by `HIVE_RELATIONAL_INDEX_BUILD_SECS`): `index_ready()`
  gates `init_schema` and `ensure_table_exists` (verified via `information_schema.tables`, never `SELECT 1 FROM t`).
  Cron is NODE-LOCAL: restore via `CronScheduler::replace_all`, never `add()`; `spawn_cron_loop` skips, never deletes,
  a due job whose project has no deployment record.

## Mesh transport, discovery, watchdogs

- `PeerPool` keys trunks by ENDPOINT ID, never the caller's label. **Every inbound accept is bounded, budgeted by its
  NEGOTIATED ALPN, and never evicts an established trunk**: `HIVE_P2P_ACCEPT_DEADLINE_MS` (40 s; past it
  `FLEET_CLOSE_OVERLOADED`, counted `accept_stuck`); pre-handshake the ClientHello picks the budget (browser 128 /
  fleet 512 / unparseable -> `pending` 256); `hive_p2p::establish_stats` is served on operator-only
  `GET /v1/mesh/establish`, never the public `/v1/mesh`.
- `vendor/iroh` keeps two patches across pin bumps: relay "read before send" and `PendingRebind` (a failed rebind is
  RETRIED; `AddrInUse` on the pinned `HIVE_IROH_PORT` otherwise leaves UDP closed). A publicly dialable address needs
  `HIVE_PUBLIC_IP` AND `HIVE_IROH_PORT`.
- **A gossip round never waits on a dead peer, and one stale reading never withdraws one.** Rounds end at
  `HIVE_GOSSIP_ROUND_DEADLINE_MS` through ONE primitive, `bounded_round::BoundedRound`; a target that keeps missing
  the deadline is waited on only `HIVE_GOSSIP_STRAGGLER_LEASH_MS`, PARKED never cancelled, keyed by ENDPOINT id. Every
  transport-failure writer funnels through `health::demote` (withdraws only after `DEMOTE_STALE_ROUNDS` 2, HOLDS while
  stalled).
- **Every automatic restart passes `ControlledRestart::request` -> `RestartReason::admissible`**: refused until
  `hive_backend::orphan_reap_ran()`, rate-capped per reason (wedge 1/6 h) from `restart_history.json`, failing CLOSED
  when unreadable; meshwatch's wedge trigger reads transport counters (`HIVE_MESH_ESTABLISH_WEDGE_SECS`), never peer
  counts.

## Containers, podman, process lifecycle

- **`KillMode=process` on hive-node.service is load-bearing** (rootful podman's conmon inherits the caller's cgroup,
  so the default SIGTERMs every managed database per roll). Tenant CELLS therefore outlive their process, so every
  boot reaps its own previous boot's cells (`hive_backend::reap_orphaned_cells`). Ownership is owner + boot
  (`hive.owner`, `hive.boot`), never boot alone; another owner's cell is never touched; every `/proc/<pid>/exe`
  compare strips the kernel's ` (deleted)` suffix (`strip_deleted_exe_suffix`).
- Every container removal is `rm -f -v` (`container_cli::rm_args`): podman takes one lock from a FIXED pool (2048) per
  container AND per volume; leaks surface as 503 `CAPACITY_EXHAUSTED`. **Never `podman volume prune`**: reclaim is
  gated on `is_anonymous_volume` (64 ascii-hex) AND `dangling=true`.

## Isolation backends & Litebox

- **Capability is PROBED and ADVERTISED, never assumed**, on the filesystem the cell execs against
  (`NodeInfo::wasm_runtime`/`bun_runtime` from `resources::RuntimeCapabilitySource`; `None` = NOT CAPABLE, unlike
  `disk_free_gb == 0`). Placement gates through ONE predicate shared by `place`, lease-stickiness and
  `dispatch_fallbacks` (`schedule::wasm_capable` and its siblings); a missing runtime is a NODE fault
  (`fault::NODE_RUNTIME_MISSING`).
- Litebox (third `CellBackend`, Firecracker -> Litebox -> Mock) is NOT Firecracker or gVisor isolation (guest and
  enforcement share one address space; seccomp-bpf is the only backstop): a node is Litebox only after
  `hive-cloud --litebox-probe` PASSES on that host with the exact staged runner AND `litebox_verified=true` on its
  `hosts.ini` line, never the flag alone, never macOS (a macOS node stays Mock). The runner has three code paths
  (fleet `litebox_runner_linux_userland`, macOS, Windows): name the variant before chasing a fork/exec symptom. Guest
  tar headers are ustar ONLY (`Header::new_ustar`: GNU longnames silently truncate the guest fs).
  `litebox-bind-shim.js` and the guest `/etc` files are inputs to `runtime_source_sha256`, so ANY change rebuilds
  every runtime artifact.
- A sandbox's live cell exists on ONE node (`SandboxRecord::owner_node`); the leader only routes. PVM kernels:
  `pti=off` is REQUIRED and `KVM_CREATE_VM` succeeding proves nothing; recall `PVM kernels on the fleet`.

## Runtime discipline (the serving runtime never stalls)

- **Heavy work never runs on a serving tokio worker.** The guardian SQL engine reloads every row of each table per
  STATEMENT, so `relational::exec` is the one chokepoint and runs under `block_in_place`; the mirror loop, index
  walker, replication writer and admin mirror reads live on the `bulkhead` runtime (`bulkhead::spawn`,
  `HIVE_BULKHEAD_THREADS` 2).
- Anything a request path reserves is released by a `Drop` guard, never only on the `Err` branch (axum drops the
  future when the client leaves): `ColdStartGuard`, `PooledConn`, `InflightSlot`, litebox guest tars, sandbox drains.
  A tenant-controlled child pipe is drained only through `hive_core::logcap::read_capped_line`, never
  `BufReader::lines()`: one newline-free line grows a `String` until the node dies.

## Builds & deploys

- `git push` auto-deploys through TWO triggers deduping on the commit SHA (`CloudState::git_poll_seen`): the webhook
  and `git::spawn_git_poll_reconcile`. **A single unreachable node never fails a deploy**: on
  `FanoutOutcome::nothing_ran()` `run_build` walks `schedule::dispatch_fallbacks` one candidate at a time with
  `place`'s `capable` + `reachable` predicates, bounded by `HIVE_DEPLOY_DISPATCH_FALLBACK_MAX` (3).
  `build interrupted: node restarted...` means the HOLDER restarted: read the TARGET's store.
- Isolated BuildExecutor: `NodeInfo::build_isolation_protocol` comes from the boot-time live probe
  `build_executor::init_installed()`; any fault logs `BUILD_ISOLATION_UNAVAILABLE`, advertises `None` and keeps the
  node out of repository-build placement: never advertise it from a flag or a literal. The sealed-artifact writer
  opens every path component O_NOFOLLOW, so a macOS node whose HIVE_DATA sits under `/var/folders` fails every build:
  use `/private/tmp`.
- Checkouts live in `git::deploy_root()`; prefix scanners cover BOTH roots, BOTH name forms and a renamed project's
  old names (`git::checkout_prefixes`, `project_settings::checkout_prefixes_including_previous`); a missing direct
  entry fails the BUILD (`git::preflight_direct_entries`), never the launch; a `fluid.json` lane runs ZERO
  install/build commands.
- `project_inventory.rs` serves `GET /v1/gitops/projects?view=summary` (tenant rows by `record_tenant`, never `norm`
  on a stored tag; leader read-through then local); the default view stays byte-identical (GitOps commits and the CLI
  depend on it). `project_redeploy` is the only path that rebuilds from `last_build`.
- Env: legacy scope `""` matches build AND runtime (`env_scope_matches`); build-inlined values (`NEXT_PUBLIC_*`) need
  `$HIVE_DATA/build-cache/*.tar` cleared before a redeploy of unchanged source: verify the SERVED bundle.
- **The host build lane's package registry is part of node health**: the lane inherits the host `/root/.npmrc`, so a
  dead mirror fails every uncached install. `build_registry::assess` probes once per build and, only for a dark KNOWN
  public mirror while registry.npmjs.org answers, hands the build a 0600 `npm_config_userconfig` copy of `~/.npmrc`
  with the public registry (a lockfile whose `resolved` URLs name the dead mirror still fails `npm ci`).
  `scripts/audit-build-registry.sh` is the fleet check; never hand-edit a node's `/etc/resolv.conf` to reproduce a DNS
  fault.

## Placement & capacity

- Disk is a HARD filter (`HIVE_PLACEMENT_DISK_FLOOR_GB`), never a score term; unknown free space
  (`NodeInfo::disk_free_gb` 0) ADMITS; placement requires an actually dialable path. **Every reclaim path needs a
  blast-radius guard**: `gc_rootfs_images` refuses an empty keep-set or an orphaned fraction over
  `HIVE_GC_MAX_REAP_FRACTION` (data images carry a DOUBLE `dpl-dpl-` prefix: match both); likewise
  `browser_artifacts::gc`, `browser_db` GC and the guardian reaper.

## Request path, circuits, cold start

- A deployment fails in two shapes: starts then dies (`crash_streak` + `last_warm_ok_ms`) or never listens
  (`warm_fail_streak`); never clear a streak because a start succeeded. A broken deployment reports
  `DEPLOYMENT_CIRCUIT_OPEN`, never `CAPACITY_EXHAUSTED`; the circuit is keyed on instance HEALTH (`unhealthy_reaps`),
  never instance count; `CIRCUIT_PROBE_INTERVAL_MS` (30 s) outlasts the failure it guards.
- Cold start: `fluid_compute::Fluid::warm` is the ONE warm primitive (`HIVE_PREWARM=0` turns the TLS-handshake prewarm
  off, not keep-warm).

## Managed data lanes

- Managed SQLite (`DbKind::Sqlite`) and `browser_db` share nothing but the word: a file per DATABASE at
  `$HIVE_DATA/sqlite-dbs/hive-sqlite-{sanitize_tag(db_id)}.db` (musql engine: same stem + `.musq`) vs a cr-sqlite CRR
  per PROJECT at `$HIVE_DATA/browser-dbs/`. **Never point the Hrana handler or musql at a `browser-dbs` file**: a bare
  `rusqlite` writer bypasses the clock tables the CRR merge reads. The SQLite lane is owner-routed, never
  leader-routed; recall `Managed SQLite (Hrana) wire contract`.
- `browser_db`: grants ride the admission, server-derived and tenant-pinned, and the fleet re-checks them on EVERY
  request (tenant+project+file derived server-side; `db_file` is an identifier, never a path); caps (`max_bytes`,
  `max_value_bytes`) bind with a typed refusal + whole-batch rollback, never truncate/evict; bytes, site ids and
  watermarks replicate ONLY through `hive_crsql` ChangeBatch (`Op::CrrSync`).
- A managed database is reachable ONLY through its `<slug>.{db_domain}` record from `host_node`'s address and
  `db_gateway` splices on that SAME node, so a host that left `registry.nodes()` STRANDS it:
  `databases::db_host_admissible` only refuses NEW placement there; stranded databases are reported
  (`db:stranded-host`), never re-hosted (re-provisioning under the same slug yields an EMPTY database).
- `db_gateway` bounds a connection only BEFORE it is established (preamble, TLS handshake, FIRST client byte and
  backend connect each under `HIVE_DB_GATEWAY_ACCEPT_DEADLINE_MS`; a `PendingGuard` caps that window per source and
  globally): an ESTABLISHED session has NO idle bound (TCP keepalive finds a vanished peer; `copy_bidirectional`
  drains each direction).

## Browser lane

- `fluid_core::browser_policy_digest` and `policyDigest` in `crates/hive-browser/www/artifact-policy.js` are ONE
  contract with TWO implementations (so are `MESH_SIG_DOMAIN` in `hive-browser` and `peer-mesh.js`: drift silently
  rejects every handshake). A browser function is eligible by fluid.json opt-in (FAILS THE BUILD when ineligible) or
  build-time auto-detection. Bytes stay node-local; only descriptor metadata replicates; delivery re-verifies size and
  source BLAKE3; admission capabilities are server-derived (`AdmissionRequest.digest` is never read, `trusted_callers`
  comes from the live HEALTHY registry); the caller's `authorization`/`cookie` never reach a donor browser.

## DNS: Seer, cutovers/ACME, custom domains

- Seer answers the fleet-served deploy zone affinity-first from the route table (`apps_host_owner`), and wildcard
  certificates cover ONE label level (~50 new certs a week per registered domain), so per-project wildcard zones cap
  out. Vercel DNS has no geo/health routing, so Seer answers only for names delegated to it and
  `desired_geo_delegation` refuses to publish below TWO nameservers. **A delegated name must never serve NEITHER
  addresses NOR delegation**: Vercel refuses NS creation while any child record exists, so a cutover is a
  restore-on-failure transaction in `vercel_dns::plan_writes` (creates before deletes, VERIFIED rollbacks). Every name
  a `desired_*` builder publishes is also in the zone's managed-name list (`platform_managed_names`), or `diff()` can
  never update or withdraw it.
- **Advertise only what peers have PROVEN**: `NodeInfo::dns_ns` is set only when `dns_bind_reaches_internet` holds
  (:53 on a wildcard bind, or a specific non-loopback address while `HIVE_PUBLIC_IP` is set; `dns_api` also needs
  `HIVE_PLATFORM_DOMAIN`) and is necessary, never sufficient: `dns_probe` proves reachability and
  `validate_nameservers` admits a node attested from TWO distinct REGIONS, never self-attested.
- Custom domains: routing keys are FULL hostnames (`fluid_gateway`'s `aliases_full`) before any first-label or
  wildcard lookup. Attachment needs observed DNS proof (the activating node itself observes `_hive-verify.<domain>`
  via DoH) through ONE helper (`mark_domain_verified`); only a VERIFIED record locks to its tenant; detach and
  activate require `require_domain_owner_if_exists` AND a stored challenge whose project matches the path. Reserved
  proof names (`_acme-challenge`, `_hive-verify`, any case) are rejected in add/import/zone-parse.

## Security gates (never weaken)

- **`auth::require_auth` proves only "some valid token"**: a mutation needs any JWT or API key, and a GET/HEAD never
  fails for missing auth (it answers `ANON_TENANT` + `x-hive-anon: 1`, scoped PER HANDLER). Every mutation and every
  tenant, operator or fleet read authorizes inside its handler (`require_project`, `require_auth_read`,
  `require_operator`, `require_fleet_read`, `require_mesh_rpc`); tenancy proves WHICH team, never WHO in it, so a
  team-scoped mutation also needs `require_team(.., min_role)` (`require_team_floor` where an unenforced dev node
  stays open). `auth::is_mesh_internal` (subject `mesh-internal` AND role `service`) is the ONE fleet-caller
  predicate, never `role == "service"` alone; a fleet-wide RPC is `require_mesh_rpc(.., Some("mesh"))`, never `None`
  (a delegation minted for ANY team passes). A handler `gossip::dispatch` also calls is mounted publicly ONLY through
  a gating wrapper (`fleet_deployments_public`; the inner handler authorizes the RESOURCE). A route that carries its
  own credential joins `auth::is_open_route` (`require_auth` and `admin_ingress` both ask it) AND fails closed itself.
- **Scope at the store primitive, not the handler**: every `WebhookStore` operation takes a `Scope`/`Audience` (a
  real team; a row without one is inert), a queue consumer may only target its own tenant's app
  (`queues::consumer_target_owned`, at registration AND every delivery; only `queues::delivery_label` reaches
  `deploy_url`), a deployment reached through a project path is authorized by `storage_broker::authorize_deployment`,
  and an API key's role is capped at its minter's (`apikeys::check_new_role`, read back through `effective_role`,
  never the mesh's `service`). Peers answer `?local=true` UNFILTERED (the billing meter's path), so every
  tenant-facing fleet aggregation re-scopes merged peer rows; node addressing reaches a non-operator only through
  `admin::non_operator_node_view`.
- **Every tenant-influenced URL the node requests goes through `hive-outbound`**, never a bare `reqwest::Client`:
  `validate_url` (https only, no userinfo, no internal name or non-public address literal) at registration AND before
  every request; `client()`/`following_client()` refuse non-public addresses at connect time (custom resolver, no
  proxy, redirects none or re-validated per hop); `same_origin_client` is for calls INTO a tenant app (preview,
  cron/workflow steps), `internal_client` (no redirect) for peers, providers and platform-built URLs. A failure shown
  to a tenant is one of `hive_outbound::describe`'s fixed sentences, never the URL, address or upstream status;
  `HIVE_OUTBOUND_ALLOW_PRIVATE=1` is a developer-only escape, REFUSED when `HIVE_JWT_SECRET` is set.
- A caller-controlled string never reaches a URL, HTML or a `Location` by interpolation: parse a trusted base, then
  `Url::set_host`/`set_path` (`state::host_scoped_https_url`, `edge::with_request_target`; `@` becomes userinfo,
  `//host` the authority, so `Url::join` needs a same-origin check, `main::invoke`); `edge::password_page` has NO form
  `action`, shown text passes `edge::html_escape`, a redirect passes `auth::same_origin_path` (twin of
  `ui/lib/same-origin-path.ts`).
- A tenant never names a loopback address: `world::world_config` accepts one only when it equals
  `DatabaseStore::local_redis_rest_url` (the project's own Redis record on this node).
- A plan is bought or granted, never self-assigned: `admin::apply_plan_everywhere(.., PlanChange)` (it writes BOTH
  `c.teams` and `c.billing`) refuses a raise without `Paid` or an operator, a REQUESTED plan resolves via
  `billing::known_plan` (never the Hobby-fallback `plan_spec`), `BillingStore::confirm_checkout(id, Payment)`
  completes an unproven payment only where `billing::mock_checkout_allowed()`, and every cents input is bounded by
  `billing::MAX_AMOUNT_CENTS` (a `u64` cast `as i64` goes NEGATIVE).
- SAML ACS verifies no assertion signature (the path team's owner/admin only); SCIM is open at the platform gate and
  authenticated ONLY by its per-team bearer (`scim_team_for_token` fails closed); `zkauth::bootstrap` binds the
  proof's ring to the project's owner (`local_owning_team`).
- A route that execs tenant commands checks the ACTIVE backend (`/v1/sandbox` refuses on Mock, which runs on the HOST
  shell), and its per-tenant cap is held by the submitted JOB, never the request's Drop guard.
- A destructive op needs an ASSERTED identity before any tenant is resolved (`has_explicit_caller_identity`: an
  unenforced node's implicit "personal" would own everything); every secret or MAC comparison is constant-time
  (`admin::ct_eq`; `==` is for ids and digests).
- **A tenant string never becomes a host path.** Podman reads `-v <a>:<b>` as a bind mount whenever `<a>` looks like a
  path, so the left side is always a platform-templated NAME (`container_volume_cfg`:
  `hive-vol-{sanitize_tag(project)}`); same for DB file names, artifact digests (64 lowercase hex, else 400) and
  snapshot dirs.
- **Tenancy comes from the cryptographically-bound JWT claim, never the spoofable `x-hive-team` header**; an untagged
  tenant is a fail-closed sentinel (`UNTAGGED_TENANT`), never collapsed to "personal". Gossip arms read the tenant
  with `mesh_tenant()` (`team_claims()`/`qparam()` underneath), never a raw `split_once`.
- Tenant processes get their env from nothing (`env_clear()` in every backend: the node env holds `HIVE_SECRET_KEY`,
  `HIVE_JWT_SECRET`, `HIVE_INTERNAL_TOKEN`, and mock-backend nodes serve real tenants). The proxy re-derives
  `x-forwarded-host`/`-proto`; the edge's client IP is the real TCP peer, never `x-forwarded-for`.
- `HIVE_SECRET_KEY` is the fleet-shared at-rest key (it also seals webhook signing secrets): never introduce or rotate
  it without the previous key in `HIVE_SECRET_KEY_OLD` (comma-separated hex); `decrypt` returns its input unchanged on
  AEAD failure, `try_decrypt` is the honest `Option` variant. Key resolution fails CLOSED
  (`secrets::load_or_create_key`, `IntegritySigner::open_or_create`): a set-but-EMPTY or malformed key, a damaged key
  file or an unpersistable one refuses to boot, never regenerates; `scripts/audit-secret-keys.sh` (repeats `hex_to_32`
  against the MainPID environ) is the PRE-ROLL check. Never print a plist, a manifest or `/proc/*/environ` wholesale:
  they carry this key. `put_env` force-masks credential-shaped values (`project_settings::looks_like_secret`)
  regardless of the UI flag.
- PQ transport: the mesh QUIC transport offers hybrid X25519MLKEM768 first via an explicit `.crypto_provider(...)` in
  `hive_p2p::bind_full`; report the REAL negotiated group (`pq_kex_stats`). **Transport IDENTITY stays classical
  ed25519.**

## Hashing: content identity vs. security binding

**BLAKE3 mints every NEW content identity** (`fluid_core::content_hash`, `ContentHasher`); `verify_content` accepts a
stored digest under BLAKE3 or the legacy hex(SHA-256) over the same domain and alone decides which algorithm it came
from (`validate_sha256`/`is_hex64` prove shape only). Digests minted before domain separation (transfer chunks,
runtime-artifact package/semantic-tree, litebox archives) are bare hashes
(`raw_content_hash`/`legacy_raw_content_sha256`/`verify_raw_content`), never mixed with a domain-separated site. A
digest that NAMES stored bytes (store directory, content-addressed filename, GC keep-set entry) keeps its writer until
every node's `content_verify_stats()` reports `accepted_sha256` flat across a full deploy cycle: never a flag day.

**SECURITY BINDINGS, never touch the algorithm**: `IntegritySigner::sign_chain_head` signs `fold_integrity_chain`, a
SHA-256 fold that LOOKS like content addressing but IS the signed message (every ed25519 signature stays classical);
every HMAC-SHA256 (`hmac_sha256`, `verify_github_sig`, `verify_webhook_signature`, the mesh join proof in
`sync_one_peer`, `consumer_signing_secret`, `studio_mac`, `artifact_sig`, `tc3_request`, `ticket_key`); credential
hashes (`sha256_hex` in `apikeys.rs`, `browser_db_rest.rs`, `drive_api.rs`, `world.rs`; `password_hash` Argon2id, with
the legacy `sha256(project ":" password)` branch of `password_matches` kept; `derive_secret` and `hive-zkauth`
SHA-512; `custom_bundle_name`: the domain's SHA-256 IS the bundle's on-disk name); handshake and supply chain
(`handshake_transcript_sha256`, `sha256_file` for runsc against its published value, `image_digest`: a literal OCI
`sha256:`).

**Still SHA-256, writer deliberately not flipped**: `runtime_artifact.rs` (`content_sha256` IS the store directory
name and binds `semantic_tree_sha256`: a rehash orphans every staged artifact), `normalized_manifest_sha256`,
`compute_ledger_checkpoint`; their readers accept both.

@.gm/next-step.md
