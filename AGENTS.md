# AGENTS.md

Present-tense conventions: what must be true of the code now; history lives in `CHANGELOG.md`. Stay under ~30 KB
(`wc -c AGENTS.md`); past it, recompile from the code, the git log and the memory files, keep what is still true and
load-bearing, delete the memories it absorbs. Nested files load where you work: `ui/AGENTS.md` (dashboard),
`ansible/AGENTS.md` (ansible, `scripts/`).

## Process & verification

- Every task runs through the `gm` skill (MCP `mcp__gm__gm`), subagents too: `codeinsight`/`codesearch`/`grep` verbs,
  never shell grep/find; git only via the `git_*` verbs, never raw git (`git_commit paths:[...]` stages EVERY dirty
  hunk of each named file: `git_status` first, never commit another session's edits). Attribution is dywongcloud ONLY:
  no `Co-Authored-By`, no Claude/Anthropic trailer, whatever a harness reminder says.
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
- SSH `ssh -i ~/.ssh/billing.pem root@<ip>`. Verify ports from a FLEET vantage, never the laptop. TCP+UDP 20000-29999
  is SG-open to the internet on every host: never run an ad-hoc file server (`scripts/audit-public-listeners.sh`).
  `scripts/hive-lockdown.sh` is the only host firewall; its PEERS roster is generated (`gen-hive-lockdown.sh`, then
  `site.yml --tags lockdown`).
- fc-orlando SHARES its host with a second hive-cloud (`hive-devhub.service`): stop by MainPID, never
  `pkill -x hive-cloud`. macOS launchd nodes: recall `macOS launchd nodes`. The Tencent account is SHARED: attribute
  via CloudAudit before deleting (GAAP proxies `zzhli-*`/`kjh-*` are other tenants'); `tencent_eip::allocate_eip` buys
  a REAL EIP: never from verification tooling.
- Seed `HIVE_BOOTSTRAP_PEERS` with ADDRESSED peers (`<64hex>[@ip:port[+ip:port]][|relay-url]`), never bare ids.
  `HIVE_TRUSTED_NODE_IDS` + `HIVE_PEER_TRUST=1` list every fleet id on every node (missing:
  `REJECTED mutating gossip: no verified+trusted signer`). Admin JWTs are HS256 on `HIVE_JWT_SECRET`, minted ON the
  node (`scripts/mint-operator-jwt.sh`; `platform_admin` needs an email in `HIVE_OWNER_EMAIL`/`HIVE_ADMIN_EMAILS`).

## Control plane & leader-only jobs

- With `HIVE_CP_OWNER_CHAIN` set the owner is the first chain entry PRESENT in `registry.nodes()` with a `peer_id` and
  public address (`Cluster::strict_chain_owner`): never the observer's `healthy` flag, never the identity election; no
  qualifying entry = HOLD. Every proxy and owner-forward asks `CloudState::leader_forward_target()`.
- **Every leader-only loop asks one question, `leadership::may_act(cloud, Job)`**: owner AND fresh view AND not
  isolated AND (backup owner only) fresh DIRECT contact with a strict majority of `HIVE_CP_VOTERS`, held for
  `Job::min_tenure()` (30 s, up to 600 s for NodeDeathRelocate); take-over keys on the ownership TERM (`TERM_LAPSE`,
  30 s). An owner present but unable to act is still owner everywhere, so every job HOLDS fleet-wide: restore
  transport/voters, never weaken the gate. Node-death self-heal is OFF unless `HIVE_NODE_DEATH_SELF_HEAL=1`.

## State: replication, routing, relational mirror

- Canonical state is ONE in-memory representation (`state.json` + store_sync), never derived from, translated through
  or gated on GuardianDB or its SQL mirror (at most a read-only projection; `guardian::spawn_restore_guard` alone
  adopts a NEWER snapshot after a regressed local file). A deserialize error in `persist::load` is silent total state
  loss: `ServiceProtocol` is strict in `FromStr` and lenient in serde, and a serde-REQUIRED key is never removed from
  a persisted struct in one release (empty it first, as `DomainRecord` does).
- `admin_ingress` forwards MUTATIONS to the leader and serves GET/HEAD LOCALLY; public hosts are round-robin DNS, so a
  GET over NODE-LOCAL data (a file under `persist::data_dir()`, an in-process `Mutex`/`OnceLock`) answers empty/404
  from the wrong node. Such a GET needs an owner/leader proxy (`build_get`, `fetch_from_host`; the owner re-checks the
  bearer and never re-proxies) plus a `gossip::dispatch` arm (longest-prefix-first; an unknown path answers an EMPTY
  body), or the state moves into `store_sync::REGISTRY` or fans out (`db_replicate::fanout_all`).
- **Store-sync adoption is WHOLESALE-REPLACE, so only from an owner every node agrees on**
  (`OwnerSource::may_adopt_wholesale_from`: the chain HEAD, or the identity election on a chain-less mesh) and never
  from an empty or unparsable payload; from a backup owner or `Fallback` only per-row `MERGE_STORES` merge. A critical
  write lands on EVERY owner-chain candidate; snapshots route through `serde_json::Value` + `enc_sorted()`.
  ProjectSettings rows replicate with per-row `updated_ms` + permanent tombstones; absence never erases a row.
- `ProjectSettings.last_build` is terminal-only: "building" is derived from the live BuildStore, and the ONLY writer
  of a build's outcome is `BuildCompletionGuard::drop`. Project delete/rename hold
  `project_settings::lifecycle_write(project)` and cascade to every node; names are unique across the PLATFORM.
- **The relational index is built by ONE background walker, never on a request path, and nothing writes the catalog
  before it completes once** (`spawn_index_refresher`, bounded by `HIVE_RELATIONAL_INDEX_BUILD_SECS`): `index_ready()`
  gates `init_schema` and `ensure_table_exists` (verified via `information_schema.tables`, never `SELECT 1 FROM t`).
  Cron is NODE-LOCAL: restore via `CronScheduler::replace_all`, never `add()`.

## Mesh transport, discovery, watchdogs

- `PeerPool` keys trunks by ENDPOINT ID, never the caller's label. **Every inbound accept is bounded, budgeted by its
  NEGOTIATED ALPN, and never evicts an established trunk**: `HIVE_P2P_ACCEPT_DEADLINE_MS` (40 s); pre-handshake the
  ClientHello picks the budget; `hive_p2p::establish_stats` is served on operator-only `GET /v1/mesh/establish`, never
  the public `/v1/mesh`.
- `vendor/iroh` keeps two patches across pin bumps (relay "read before send", `PendingRebind`); a publicly dialable
  address needs `HIVE_PUBLIC_IP` AND `HIVE_IROH_PORT`.
- **A gossip round never waits on a dead peer, and one stale reading never withdraws one.** Rounds end at
  `HIVE_GOSSIP_ROUND_DEADLINE_MS` through ONE primitive, `bounded_round::BoundedRound`; a target that keeps missing
  the deadline is waited on only `HIVE_GOSSIP_STRAGGLER_LEASH_MS`, PARKED never cancelled, keyed by ENDPOINT id. Every
  transport-failure writer funnels through `health::demote` (withdraws only after `DEMOTE_STALE_ROUNDS`, HOLDS while
  stalled).
- **Every automatic restart passes `ControlledRestart::request` -> `RestartReason::admissible`**: refused until
  `hive_backend::orphan_reap_ran()`, rate-capped per reason (wedge 1/6 h) from `restart_history.json`, failing CLOSED
  when unreadable.

## Containers, podman, process lifecycle

- **`KillMode=process` on hive-node.service is load-bearing** (rootful podman's conmon inherits the caller's cgroup,
  so the default SIGTERMs every managed database per roll). Tenant CELLS therefore outlive their process, so every
  boot reaps its own previous boot's cells (`hive_backend::reap_orphaned_cells`). Ownership is owner + boot
  (`hive.owner`, `hive.boot`), never boot alone; another owner's cell is never touched.
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
  `hosts.ini` line, never the flag alone, never macOS. `litebox-bind-shim.js` and the guest `/etc` files feed
  `runtime_source_sha256`, so ANY change rebuilds every runtime artifact.
- A sandbox's live cell exists on ONE node (`SandboxRecord::owner_node`); the leader only routes. PVM kernels:
  `pti=off` is REQUIRED and `KVM_CREATE_VM` succeeding proves nothing; recall `PVM kernels on the fleet`.

## Runtime discipline (the serving runtime never stalls)

- **Heavy work never runs on a serving tokio worker.** The guardian SQL engine reloads every row of each table per
  STATEMENT, so `relational::exec` is the one chokepoint and runs under `block_in_place`; the mirror loop, index
  walker, replication writer and admin mirror reads live on the `bulkhead` runtime (`bulkhead::spawn`).
- Model downloads share ONE node-wide slot count (`HIVE_INFERENCE_MODEL_DOWNLOADS_MAX` 2), one disk ledger
  (`DiskClaim`) and a per-tenant slot (`TenantSlot`, 1), and a transfer under `HIVE_INFERENCE_MODEL_MIN_BYTES_PER_SEC`
  over its rate window is cut: a slow drip never trips a stall timeout, so one tenant's URLs could hold every slot for
  hours.
- Anything a request path reserves is released by a `Drop` guard, never only on the `Err` branch (axum drops the
  future when the client leaves): `ColdStartGuard`, `PooledConn`, `InflightSlot`, litebox guest tars, sandbox drains.
  A tenant-controlled child pipe is drained only through `hive_core::logcap::read_capped_line`, never
  `BufReader::lines()`: one newline-free line grows a `String` until the node dies.

## Builds & deploys

- `git push` auto-deploys through TWO triggers deduping on the commit SHA (`CloudState::git_poll_seen`): the webhook
  and `git::spawn_git_poll_reconcile`. **A single unreachable node never fails a deploy**: on
  `FanoutOutcome::nothing_ran()` `run_build` walks `schedule::dispatch_fallbacks` one candidate at a time with
  `place`'s `capable` + `reachable` predicates, bounded by `HIVE_DEPLOY_DISPATCH_FALLBACK_MAX` (3).
- Isolated BuildExecutor: `NodeInfo::build_isolation_protocol` comes from the boot-time live probe
  `build_executor::init_installed()`; any fault logs `BUILD_ISOLATION_UNAVAILABLE`, advertises `None` and keeps the
  node out of repository-build placement: never advertise it from a flag or a literal.
- Checkouts live in `git::deploy_root()`; prefix scanners cover BOTH roots, BOTH name forms and a renamed project's
  old names (`git::checkout_prefixes`); a missing direct entry fails the BUILD (`git::preflight_direct_entries`),
  never the launch; a `fluid.json` lane runs ZERO install/build commands.
- The default `GET /v1/gitops/projects?view=summary` stays byte-identical (GitOps commits and the CLI depend on it);
  `project_redeploy` alone rebuilds from `last_build`.
- Env: legacy scope `""` matches build AND runtime (`env_scope_matches`); build-inlined values (`NEXT_PUBLIC_*`) need
  `$HIVE_DATA/build-cache/*.tar` cleared before a redeploy of unchanged source: verify the SERVED bundle.
- **The host build lane's package registry is part of node health**: it inherits the host `/root/.npmrc`, so a dead
  mirror fails every uncached install; `build_registry::assess` swaps in a 0600 `npm_config_userconfig` with the
  public registry, only for a dark KNOWN public mirror while registry.npmjs.org answers
  (`scripts/audit-build-registry.sh` is the fleet check).

## Placement & capacity

- Disk is a HARD filter (`HIVE_PLACEMENT_DISK_FLOOR_GB`), never a score term; unknown free space
  (`NodeInfo::disk_free_gb` 0) ADMITS; placement requires an actually dialable path. **Every reclaim path needs a
  blast-radius guard**: `gc_rootfs_images` refuses an empty keep-set or an orphaned fraction over
  `HIVE_GC_MAX_REAP_FRACTION` (data images carry a DOUBLE `dpl-dpl-` prefix: match both).

## Request path, circuits, cold start

- A deployment fails in two shapes: starts then dies (`crash_streak` + `last_warm_ok_ms`) or never listens
  (`warm_fail_streak`); never clear a streak because a start succeeded. A broken deployment reports
  `DEPLOYMENT_CIRCUIT_OPEN`, never `CAPACITY_EXHAUSTED`; the circuit is keyed on instance HEALTH (`unhealthy_reaps`),
  never instance count.

## Managed data lanes

- Managed SQLite (`DbKind::Sqlite`) and `browser_db` share nothing but the word: a file per DATABASE at
  `$HIVE_DATA/sqlite-dbs/hive-sqlite-{sanitize_tag(db_id)}.db` vs a cr-sqlite CRR per PROJECT at
  `$HIVE_DATA/browser-dbs/`. **Never point the Hrana handler or musql at a `browser-dbs` file**: a bare `rusqlite`
  writer bypasses the clock tables the CRR merge reads. The SQLite lane is owner-routed, never leader-routed; recall
  `Managed SQLite (Hrana) wire contract`.
- `browser_db`: grants ride the admission, server-derived and tenant-pinned, and the fleet re-checks them on EVERY
  request (tenant+project+file derived server-side; `db_file` is an identifier, never a path); caps (`max_bytes`,
  `max_value_bytes`) bind with a typed refusal + whole-batch rollback, never truncate/evict; bytes, site ids and
  watermarks replicate ONLY through `hive_crsql` ChangeBatch (`Op::CrrSync`).
- A managed database is reachable ONLY through its `<slug>.{db_domain}` record from `host_node`'s address and
  `db_gateway` splices on that SAME node, so a host that left `registry.nodes()` STRANDS it:
  `databases::db_host_admissible` only refuses NEW placement; stranded databases are reported, never re-hosted
  (re-provisioning under the same slug yields an EMPTY database).
- `db_gateway` bounds a connection while it is UNPROVEN or DRAINING, never proven and idle: proven = its database
  ACCEPTED AUTHENTICATION (`AuthProof`: Postgres `AuthenticationOk`, Redis `+OK` to `AUTH`/`HELLO`; `-NOAUTH` and
  `+RESET` prove nothing), within `HIVE_DB_GATEWAY_ACCEPT_DEADLINE_MS` before the backend and
  `HIVE_DB_GATEWAY_AUTH_DEADLINE_MS` (30 s, absolute) after; a full `PendingTable` EVICTS the oldest stuck connection
  so a few addresses cannot starve the rest; `splice` copies each direction itself, never `copy_bidirectional` or a
  `select!` over two copies.

## Browser lane

- `fluid_core::browser_policy_digest` and `policyDigest` in `crates/hive-browser/www/artifact-policy.js` are ONE
  contract with TWO implementations (so are `MESH_SIG_DOMAIN` in `hive-browser` and `peer-mesh.js`: drift silently
  rejects every handshake).

## DNS: Seer, cutovers/ACME, custom domains

- Seer answers the fleet-served deploy zone affinity-first from the route table (`apps_host_owner`). Vercel DNS has no
  geo/health routing, so Seer answers only for names delegated to it and `desired_geo_delegation` refuses to publish
  below TWO nameservers. **A delegated name must never serve NEITHER addresses NOR delegation**: Vercel refuses NS
  creation while any child record exists, so a cutover is a restore-on-failure transaction in
  `vercel_dns::plan_writes` (creates before deletes, VERIFIED rollbacks).
- **Advertise only what peers have PROVEN**: `NodeInfo::dns_ns` is set only when `dns_bind_reaches_internet` holds and
  is necessary, never sufficient: `dns_probe` proves reachability and `validate_nameservers` admits a node attested
  from TWO distinct REGIONS, never self-attested.
- A platform-wide public-DNS question (the `www` SAN, the `_hive-verify` TXT proof) is asked of `dns_quorum::VANTAGES`
  and admitted by `Tally::admits`: a resolver that errors ABSTAINS (never a "no"), a MAJORITY of the ANSWERING
  operators is required with a floor of 2, and the fleet's own nameservers are never a vantage (self-attestation).
- Custom domains: routing keys are FULL hostnames (`fluid_gateway`'s `aliases_full`) before any first-label or
  wildcard lookup. Attachment needs observed DNS proof (the activating node itself observes `_hive-verify.<domain>`)
  through ONE helper (`mark_domain_verified`); only a VERIFIED record locks to its tenant; detach and activate require
  `require_domain_owner_if_exists` AND a stored challenge whose project matches the path. Reserved proof names
  (`_acme-challenge`, `_hive-verify`) are rejected in add/import/zone-parse.

## Security gates (never weaken)

- **`auth::require_auth` proves only "some valid token"** (a GET never fails for missing auth: it answers
  `ANON_TENANT`, scoped PER HANDLER); a ROLE is decided once, by `roles::RULES`, which `require_auth` asks
  (`roles::gate`) for every authenticated request on an enforced node, reads included: viewer < member < admin <
  owner, a read is viewer and a write NOBODY LISTED is ADMIN, so a new route is admin until listed (billing
  checkout/charge, DNS, database delete/rotate, WAF, rate limit, bot, SMS providers and the repository link are admin
  rows). A handler never re-decides a role (`admin::require_team` only binds the path slug; ownership still authorizes
  inside it) and `ui/lib/team-role.ts` mirrors `RULES` row for row. `owner` owns only the caller's OWN tenant: a gate
  opens on a tenant match or `platform_admin`, never a role (`edge::claims_open_preview`).
- A `service` role STRING opens nothing: `auth::verify` accepts a `service` claim only for a FLEET identity
  (`auth::is_fleet_service`: `mesh-internal` or `mesh-node:<coordinator>`, subject AND role), every other one is no
  principal; a fleet-wide RPC is `require_mesh_rpc(.., Some("mesh"))`, never `None`. A handler `gossip::dispatch` also
  calls is mounted publicly ONLY through a gating wrapper (`fleet_deployments_public`; the inner handler authorizes
  the RESOURCE). A route with its own credential joins `auth::is_open_route` (`admin_ingress` asks it too) AND fails
  closed itself.
- The workflow World's queue token (`HIVE_QUEUE_TOKEN`) is the one tenant-held bearer: `auth::verify_queue_client`
  (subject AND role, never `platform_admin`, `iat` within `HIVE_QUEUE_TOKEN_MAX_AGE_SECS`, 30 d) is resolved by
  `auth::principal`, the ONE resolution `require_auth` and `admin_ingress` share, and confined by
  `roles::QUEUE_CLIENT_ROUTES` to enqueue + stats. It sits in a tenant's env, so `world_queue::rotate_queue_tokens`
  re-issues an aged or sealed one in each local deployment's function env about 90 s after boot and every 6 h
  (`HIVE_QUEUE_TOKEN_ROTATE_SECS`), as a PLAINTEXT token (a project row's `enc:v1:` copy is no bearer).
- A project transfer is a MOVE: the settings row is the owner of record (`admin::row_holder`; the copies are evidence
  only for a row with NO tag), so a read keyed by a DEPLOYMENT ID (integrity chain, build, resources) asks ONE
  predicate, `admin::deployment_owned_by`, never an inline copy and never a project NAME or team SLUG compared with a
  tenant (a slug is whatever its creator types; `record_tenant` takes a stored tag). A claim never takes a project
  (`ProjectStore::claim_team` is a compare-and-set), and deployments follow the row
  (`Gateway::retag_project`, then every host in `tenancy_reconcile::retag_deployments_to_rows`:
  `HIVE_DEPLOYMENT_RETAG_SECS` 15, `HIVE_DEPLOYMENT_RETAG=0` disables; its first pass after a roll re-tags earlier
  transfers). A move needs a destination the caller administers (`caller_administers`) or the operator.
- Every roster change goes through `TeamStore::set_member_role`/`remove_member_as` (`check_roster_change`: nobody
  changes, removes or grants above their own role, the last Owner stays), and `remove_checked` authorises EVERY row a
  removal deletes (`check_roster_removal`), so a ghost row ahead of the Owner cannot carry the Owner away; a directory
  sync only adds (`add_member_if_absent`) or removes a plain member (`remove_plain_member`).
- Only the dashboard SERVER writes the identity mirror (`identity_sync` under `mint_allowed`): it feeds `team_create`
  and `caller_administers`, so a browser-originated sync records nothing.
- `POST /v1/token` checks the internal token FIRST: only a failing request spends the strict per-peer guess budget
  (`mint_rate_limiter`, 20/min); a legitimate mint spends a per-USER budget (`HIVE_MINT_USER_PER_MIN` 30) and a
  peer-wide backstop (`HIVE_MINT_BACKSTOP_PER_MIN` 600), since the dashboard server is one peer for every user. A
  token carries its role for its whole life: `admin`/`member`/`viewer` live `HIVE_ORG_TOKEN_TTL_SECS` (900 s), the
  rest 3600 s. Residual: the strict budget changes only the answer (a correct guess still mints; the 600/min
  admin-router limit is the only bound), and ~20 colluding accounts (30/min each) exhaust the backstop.
- **Scope at the store primitive, not the handler**: every `WebhookStore` operation takes a `Scope`/`Audience`, a
  queue consumer may only target its own tenant's app (`queues::consumer_target_owned`, at registration AND every
  delivery), a deployment reached through a project path is authorized by `storage_broker::authorize_deployment`, an
  API key's role is capped at its minter's (`apikeys::check_new_role`). Peers answer `?local=true` UNFILTERED, so a
  tenant-facing fleet aggregation re-scopes merged peer rows (node addressing: `admin::non_operator_node_view`).
- **Every tenant-influenced URL the node requests goes through `hive-outbound`**, never a bare `reqwest::Client`:
  `validate_url` (https only, no userinfo, no internal name or non-public address literal) at registration AND before
  every request; `client()`/`following_client()` refuse non-public addresses at connect time (no proxy, redirects none
  or re-validated per hop); `same_origin_client` is for calls INTO a tenant app, `internal_client` (no redirect) for
  peers and platform-built URLs. A failure shown to a tenant is one of `hive_outbound::describe`'s fixed sentences;
  `HIVE_OUTBOUND_ALLOW_PRIVATE=1` is a developer-only escape, REFUSED when `HIVE_JWT_SECRET` is set.
- A caller-controlled string never reaches a URL, HTML or a `Location` by interpolation: parse a trusted base, then
  `Url::set_host`/`set_path` (`state::host_scoped_https_url`, `edge::with_request_target`; `//host` is an authority,
  so `Url::join` needs a same-origin check, `main::invoke`); `edge::password_page` has NO form `action`, shown text
  passes `edge::html_escape`, a redirect passes `auth::same_origin_path`.
- A tenant never names a loopback address: `world::world_config` accepts one only when it equals the REST URL of one
  of the project's own Redis records on this node (`DatabaseStore::local_redis_rest_urls`).
- A plan is bought or granted, never self-assigned: `admin::apply_plan_everywhere(.., PlanChange)` (it writes BOTH
  `c.teams` and `c.billing`) refuses a raise without `Paid` or an operator, a REQUESTED plan resolves via
  `billing::known_plan` (never `plan_spec`'s Hobby fallback), `BillingStore::confirm_checkout(id, Payment)` completes
  an unproven payment only where `billing::mock_checkout_allowed()` and a Stripe one only when
  `billing::session_paid`, and every cents input is bounded by `billing::MAX_AMOUNT_CENTS` (a `u64` cast `as i64` goes
  NEGATIVE).
- SAML ACS verifies no assertion signature, so only the path team's owner/admin may post it (`require_team_floor`);
  SCIM is open at the platform gate, managed by an owner/admin and authenticated ONLY by its per-team bearer
  (`scim_team_for_token` fails closed); `zkauth::bootstrap` binds the proof's ring to the project's owner
  (`local_owning_team`).
- A route that execs tenant commands checks the ACTIVE backend (`/v1/sandbox` refuses on Mock, which runs on the HOST
  shell), and its per-tenant cap is held by the submitted JOB, never the request's Drop guard.
- A cookie, token or stored password that DECIDES access is read with `secrets::try_decrypt`, never the passthrough
  `decrypt`; the password-gate unlock cookie is a server-sealed `v1.<expiry ms>.<HMAC>` over (deployment, expiry,
  stored hash) (`EnterpriseStore::seal_password_cookie`, `HIVE_PW_COOKIE_TTL_SECS`), `Secure` unless plain-http
  loopback (`edge::unlock_cookie_header`).
- A destructive op needs an ASSERTED identity before any tenant is resolved (`has_explicit_caller_identity`: an
  unenforced node's implicit "personal" would own everything); every secret or MAC comparison is constant-time
  (`admin::ct_eq`; `==` is for ids and digests).
- **A tenant string never becomes a host path.** Podman reads `-v <a>:<b>` as a bind mount whenever `<a>` looks like a
  path, so the left side is always a platform-templated NAME (`container_volume_cfg`); same for DB file names,
  artifact digests (64 lowercase hex, else 400) and snapshot dirs.
- **Tenancy comes from the cryptographically-bound JWT claim, never the spoofable `x-hive-team` header**; an untagged
  tenant is a fail-closed sentinel (`UNTAGGED_TENANT`), never collapsed to "personal". Gossip arms read the tenant
  with `mesh_tenant()`, never a raw `split_once`.
- Tenant processes get their env from nothing (`env_clear()` in every backend: the node env holds `HIVE_SECRET_KEY`,
  `HIVE_JWT_SECRET`, `HIVE_INTERNAL_TOKEN`, and mock-backend nodes serve real tenants). The proxy re-derives
  `x-forwarded-host`/`-proto`; the edge's client IP is the real TCP peer, never `x-forwarded-for`.
- `HIVE_SECRET_KEY` is the fleet-shared at-rest key: never introduce or rotate it without the previous key in
  `HIVE_SECRET_KEY_OLD` (comma-separated hex); `decrypt` returns its input unchanged on AEAD failure, `try_decrypt` is
  the honest `Option` variant. Key resolution fails CLOSED (`secrets::load_or_create_key`,
  `IntegritySigner::open_or_create`): a set-but-EMPTY or malformed key, a damaged key file or an unpersistable one
  refuses to boot, never regenerates; `scripts/audit-secret-keys.sh` (repeats `hex_to_32` against the MainPID environ)
  is the PRE-ROLL check. Never print a plist, a manifest or `/proc/*/environ` wholesale: they carry this key.
  `put_env` force-masks credential-shaped values (`project_settings::looks_like_secret`).
- PQ transport: the mesh QUIC transport offers hybrid X25519MLKEM768 first via an explicit `.crypto_provider(...)` in
  `hive_p2p::bind_full` (report the REAL group, `pq_kex_stats`); **transport IDENTITY stays classical ed25519.**

## Hashing: content identity vs. security binding

**BLAKE3 mints every NEW content identity** (`fluid_core::content_hash`, `ContentHasher`); `verify_content` accepts a
stored digest under BLAKE3 or the legacy hex(SHA-256) over the same domain and alone decides which algorithm it came
from. Pre-domain-separation digests are bare hashes (`raw_content_hash`/`verify_raw_content`), never mixed with a
domain-separated site. A digest that NAMES stored bytes keeps its writer until every node's `content_verify_stats()`
reports `accepted_sha256` flat across a full deploy cycle (`runtime_artifact.rs`'s `content_sha256` IS the store
directory name: a rehash orphans every staged artifact).

**SECURITY BINDINGS, never touch the algorithm**: `IntegritySigner::sign_chain_head` signs `fold_integrity_chain`, a
SHA-256 fold that LOOKS like content addressing but IS the signed message (every ed25519 signature stays classical);
every HMAC-SHA256 (`hmac_sha256`, `verify_github_sig`, `verify_webhook_signature`, `sync_one_peer`'s join proof,
`consumer_signing_secret`, `tc3_request`, `ticket_key`); credential hashes (`sha256_hex` in
`apikeys.rs`/`browser_db_rest.rs`/`drive_api.rs`/`world.rs`, `password_hash` Argon2id with the legacy
`sha256(project ":" password)` branch of `password_matches` kept, `derive_secret`, `hive-zkauth`,
`custom_bundle_name`); handshake and supply chain (`handshake_transcript_sha256`, `sha256_file` for runsc,
`image_digest`: a literal OCI `sha256:`).

@.gm/next-step.md
