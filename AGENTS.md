# AGENTS.md

Present tense: what is true now; history in `CHANGELOG.md`. Past ~30 KB, recompile from code:
keep load-bearing, drop absorbed. Nested:
`ui/AGENTS.md`, `ui/components/landing/AGENTS.md`, `ansible/AGENTS.md` (ansible, `scripts/`).

## Process & verification

- Every task runs through the `gm` skill (`mcp__gm__gm`), subagents too: `codeinsight`/`codesearch`/`grep` verbs, never shell
grep/find; git only via `git_*` verbs (`git_commit paths:[...]` stages EVERY dirty hunk of a named file: `git_status` first).
Attribution is dywongcloud ONLY: no `Co-Authored-By`, no Claude/Anthropic trailer.
- **No test files, ever** (no `test/`, `__tests__/`, `spec/`, `#[cfg(test)]`, mocking libraries). `cargo test
--workspace --locked` + `scripts/cli-acceptance.sh` (macOS `/private/tmp`) + live execution; fix a failure CLASS at the
shared primitive. `cargo check` skips `cfg(target_os="linux")` AND test targets, so run, not `--no-run`; a dep change ships
`Cargo.lock` (`--locked`). `scripts/check-contract-bindings.py --check` gates CI; drift names `--sync`, never in CI.
`docs/ci-contract-bindings.md`.
- Cargo/disk traps: keep the user's PATH (Homebrew cargo has no cross std); `hive-cloud` is bin-only; `df` before a build; never
build in `ui/`, never kill fc-lax. A stray `refs/heads/origin/main` makes `origin/main` ambiguous: count unpushed work via
`refs/remotes/origin/main..HEAD`. Never commit `ansible/inventory/hosts.ini` or ANY backup name form (removing a pushed
copy needs a force-push rewrite). `hive_health_interval` > `hive_health_timeout`; an empty reviewed public resolver set
fails build-executor preflight; raise `deploy_serial_backend` only with quorum-safety evidence.
- A roll/deploy is a production change: operator's word first, then `ansible/AGENTS.md` (dashboard: `ui/AGENTS.md`);
prove the RUNNING binary with `strings -a /proc/<MainPID>/exe | grep -c '<log text>'`, never by grepping a Rust format literal;
never `git_reword` during a roll (rewrites every later SHA).

## Fleet, hosts, access

- Live roster = non-`hive_retired` lines of the operator-owned, git-ignored `ansible/inventory/hosts.ini`: fc-orlando
209.23.0.183, va, va3, sj (chain head), sj7, phx. `[platform:vars]` templates
`HIVE_CP_OWNER_CHAIN=fc-sanjose,fc-phoenix,fc-virginia-3,fc-virginia` and `HIVE_CP_VOTERS=fc-sanjose,fc-phoenix,fc-virginia-3`; a
node drop-in overrides the unit: read `/proc/<pid>/environ`.
- SSH `ssh -i ~/.ssh/id_ed25519 root@<ip>` (billing.pem refused); fc-orlando needs `sysadmin@` + `sudo -n`.
Admin API is plain HTTP loopback :8786, not :8787/https. Verify ports from a FLEET vantage, never the laptop,
and serve a URL from the node to itself first (a vendor policy-drop hides a fully-serving node). TCP+UDP
20000-29999 is SG-open: never run an ad-hoc file server. `scripts/hive-lockdown.sh` is the sole host firewall;
PEERS is generated (`gen-hive-lockdown.sh`, `site.yml --tags lockdown`). A priority -95 accept table does NOT
stop a later policy-drop base chain: put the accept in the chain INPUT jumps to.
- fc-orlando SHARES its host with a second hive-cloud (`hive-devhub.service`): stop by MainPID, never `pkill -x hive-cloud`, or
DevHub's `/api/*` 500s. The Tencent account is SHARED: attribute via CloudAudit before deleting (GAAP proxies `zzhli-*`/`kjh-*` are
other tenants'); `tencent_eip::allocate_eip` buys a REAL EIP: never from verification tooling.
- Seed `HIVE_BOOTSTRAP_PEERS` with ADDRESSED peers (`<64hex>[@ip:port[+ip:port]][|relay-url]`), never bare ids.
`HIVE_TRUSTED_NODE_IDS` + `HIVE_PEER_TRUST=1` list every fleet id on every node (missing: `REJECTED mutating gossip: no
verified+trusted signer`). Admin JWTs are HS256 on `HIVE_JWT_SECRET`, minted ON the node (`scripts/mint-operator-jwt.sh`);
`platform_admin` needs an email in `HIVE_OWNER_EMAIL`/`HIVE_ADMIN_EMAILS`.

## Control plane & leader-only jobs

- With `HIVE_CP_OWNER_CHAIN` set the owner is the first chain entry PRESENT in `registry.nodes()` with a `peer_id` and public address
(`Cluster::strict_chain_owner`): never the observer's `healthy` flag, never the identity election; no qualifying entry = HOLD. Every
proxy and owner-forward asks `CloudState::leader_forward_target()`.
- **Every leader-only loop asks one question, `leadership::may_act(cloud, Job)`**: owner AND fresh view AND not isolated AND (backup
owner only) fresh DIRECT contact with a strict majority of `HIVE_CP_VOTERS`, held for `Job::min_tenure()` (30 s, 600 s for
NodeDeathRelocate); take-over keys on the ownership TERM (`TERM_LAPSE`, 30 s). An owner present but unable to act is still owner
everywhere, so every job HOLDS fleet-wide: restore transport/voters, never weaken the gate. Node-death self-heal is OFF unless
`HIVE_NODE_DEATH_SELF_HEAL=1`.
- `RATE_CARD` > `COST_CARD` (>=6000 bps, `assert_profitable()`); ONE `may_act(BillingMeter)` writer;
charging OFF until `HIVE_BILLING_METER_RESOURCES=1`.

## State: replication, routing, relational mirror

- Canonical state is ONE in-memory representation (`state.json` + store_sync), never via GuardianDB or its SQL mirror (at
most a read-only projection; `guardian::spawn_restore_guard` alone adopts a NEWER snapshot). A deserialize error in
`persist::load` is silent total state loss: `ServiceProtocol` is strict in `FromStr`, lenient in serde, and a serde-REQUIRED
key is never removed from a persisted struct in one release.
- `admin_ingress` forwards MUTATIONS to the leader and serves GET/HEAD LOCALLY; public hosts are round-robin DNS, so a GET
over NODE-LOCAL data (a file under `persist::data_dir()`, an in-process `Mutex`/`OnceLock`) answers empty/404 from the wrong
node. Such a GET needs an owner proxy (`build_get`, `fetch_from_host`; the owner re-checks the bearer, never re-proxies)
plus a `gossip::dispatch` arm (longest-prefix-first; unknown path = EMPTY body), or the state moves into
`store_sync::REGISTRY` or fans out (`db_replicate::fanout_all`).
- **Store-sync adoption is WHOLESALE-REPLACE, so only from an owner every node agrees on**
(`OwnerSource::may_adopt_wholesale_from`: chain HEAD, or the identity election on a chain-less mesh), never from an empty or
unparsable payload; from a backup owner or `Fallback` only per-row `MERGE_STORES`. A critical write lands on EVERY owner-chain
candidate; snapshots route through `serde_json::Value` + `enc_sorted()`. ProjectSettings rows replicate with per-row `updated_ms` +
permanent tombstones; absence never erases a row.
- `ProjectSettings.last_build` is terminal-only: "building" comes from the live BuildStore, and the ONLY writer of a
build's outcome is `BuildCompletionGuard::drop`. Project delete/rename hold `project_settings::lifecycle_write(project)`
and cascade to every node; names are unique across the PLATFORM and a rename is IRREVERSIBLE (old name = permanent
case-insensitive tombstone, 409 pre-auth): confirm with the operator first, and non-empty `moved_domains` is the go/no-go gate.
- **The relational index is built by ONE background walker, never on a request path, and nothing writes the catalog before
it completes once** (`spawn_index_refresher`, `HIVE_RELATIONAL_INDEX_BUILD_SECS`): `index_ready()` gates `init_schema`
and `ensure_table_exists` (`information_schema.tables`, never `SELECT 1 FROM t`). A non-CAS catalog write LOSES tables
and no restart repairs it: CAS the doc, repair 42P01, drop orphan indexes. Cron is NODE-LOCAL: `CronScheduler::replace_all`,
never `add()`. fc-orlando is NOT enrolled in the fail2ban role; `site.yml --tags fail2ban` is the sanctioned apply and
needs the operator word.

## Mesh transport, discovery, watchdogs

- `PeerPool` keys trunks by ENDPOINT ID, never the caller's label. **Every inbound accept is bounded, budgeted by its
NEGOTIATED ALPN, and never evicts an established trunk**: `HIVE_P2P_ACCEPT_DEADLINE_MS` (40 s); the pre-handshake
ClientHello picks the budget; `establish_stats` is served only on operator `GET /v1/mesh/establish`, never `/v1/mesh`.
- `vendor/iroh` keeps two patches across pin bumps (relay "read before send", `PendingRebind`); a publicly dialable
address needs `HIVE_PUBLIC_IP` AND `HIVE_IROH_PORT`.
- **A gossip round never waits on a dead peer, and one stale reading never withdraws one.** Rounds end at
`HIVE_GOSSIP_ROUND_DEADLINE_MS` through ONE primitive, `bounded_round::BoundedRound`; a target that keeps missing the
deadline is waited on only `HIVE_GOSSIP_STRAGGLER_LEASH_MS`, PARKED never cancelled, keyed by ENDPOINT id. Every
transport-failure writer funnels through `health::demote` (withdraws only after `DEMOTE_STALE_ROUNDS`, HOLDS while stalled).
- **Every automatic restart passes `ControlledRestart::request` -> `RestartReason::admissible`**: refused until
`hive_backend::orphan_reap_ran()`, rate-capped per reason (wedge 1/6 h) from `restart_history.json`, fails CLOSED if unreadable.

## Containers, podman, process lifecycle

- **`KillMode=process` on hive-node.service is load-bearing** (rootful podman's conmon inherits the caller's cgroup:
the default SIGTERMs every managed database per roll). CELLS outlive their process, so every boot reaps its own previous
boot's cells (`hive_backend::reap_orphaned_cells`). Ownership is owner + boot (`hive.owner`, `hive.boot`), never boot alone.
- Every container removal is `rm -f -v` (`container_cli::rm_args`): podman takes one lock from a FIXED pool (2048) per
container AND per volume; leaks surface as 503 `CAPACITY_EXHAUSTED`. **Never `podman volume prune`**: reclaim is gated
on `is_anonymous_volume` (64 ascii-hex) AND `dangling=true`. Podman runs `short-name-mode=enforcing`, so tenant `FROM`
refs must be qualified.

## Isolation backends & Litebox

- **Capability is PROBED and ADVERTISED, never assumed**, on the filesystem the cell execs against
(`NodeInfo::wasm_runtime`/`bun_runtime` from `RuntimeCapabilitySource`; `None` = NOT CAPABLE, unlike `disk_free_gb == 0`).
Placement gates through ONE predicate shared by `place`, lease-stickiness and `dispatch_fallbacks` (`schedule::wasm_capable`);
a missing runtime is a NODE fault (`fault::NODE_RUNTIME_MISSING`).
- Litebox (Firecracker -> Litebox -> Mock) is NOT Firecracker/gVisor isolation (guest and enforcement share one address
space; seccomp-bpf is the only backstop): a node is Litebox only after `hive-cloud --litebox-probe` PASSES on that host with
the exact staged runner AND `litebox_verified=true` on its `hosts.ini` line, never the flag alone, never macOS.
`litebox-bind-shim.js` and guest `/etc` feed `runtime_source_sha256`, so ANY change rebuilds every artifact.
- A sandbox's live cell exists on ONE node (`SandboxRecord::owner_node`); the leader only routes.

## Runtime discipline (the serving runtime never stalls)

- **Heavy work never runs on a serving tokio worker.** The guardian SQL engine reloads every row per STATEMENT:
`relational::exec` is the one chokepoint, under `block_in_place` -- a tokio timeout CANNOT preempt it, so a request-path read is
bounded by ONE deadline per statement (`exec_with_budget` + `budget_left`); prove a budget works by its miss-WARN firing,
not by the code existing. Mirror loop, index walker, replication writer and admin mirror reads live on `bulkhead`.
- Model downloads share ONE node-wide slot count (`HIVE_INFERENCE_MODEL_DOWNLOADS_MAX` 2), one `DiskClaim` and one `TenantSlot`; a slow
drip never trips a stall timeout.
- Anything a request path reserves is released by a `Drop` guard, never only on the `Err` branch (axum drops the future when the
client leaves): `ColdStartGuard`, `PooledConn`, `InflightSlot`, litebox guest tars, sandbox drains. A tenant-controlled child pipe is
drained only through `hive_core::logcap::read_capped_line`, never `BufReader::lines()`: one newline-free line grows a `String` forever.

## Builds & deploys

- `git push` auto-deploys through TWO triggers deduping on the commit SHA (`CloudState::git_poll_seen`): the webhook
and `git::spawn_git_poll_reconcile`. **A single unreachable node never fails a deploy**: on `FanoutOutcome::nothing_ran()`
`run_build` walks `schedule::dispatch_fallbacks` one candidate at a time with `place`'s `capable` + `reachable`
predicates, bounded by `HIVE_DEPLOY_DISPATCH_FALLBACK_MAX` (3).
- Isolated BuildExecutor: `NodeInfo::build_isolation_protocol` comes from the boot-time live probe
`build_executor::init_installed()`; any fault logs `BUILD_ISOLATION_UNAVAILABLE`, advertises `None` and drops the node from
repository-build placement.
- Checkouts live in `git::deploy_root()`; prefix scanners cover BOTH roots, BOTH name forms and a renamed project's old names
(`git::checkout_prefixes`); a missing direct entry fails the BUILD (`git::preflight_direct_entries`), never the launch.
- The default `GET /v1/gitops/projects?view=summary` stays byte-identical (GitOps commits and the CLI depend on it);
`project_redeploy` alone rebuilds from `last_build`. Env: legacy scope `""` matches build AND runtime (`env_scope_matches`);
build-inlined values (`NEXT_PUBLIC_*`) need `$HIVE_DATA/build-cache/*.tar` cleared before a redeploy of unchanged source: verify the
SERVED bundle.
- **The host build lane's package registry is part of node health**: it inherits the host `/root/.npmrc`, so a dead
mirror fails every uncached install; `build_registry::assess` swaps in a 0600 `npm_config_userconfig` with the public
registry, only for a dark KNOWN public mirror while registry.npmjs.org answers.
- **Credentials resolve SERVER-SIDE**: the GitHub connection rides in an ENCRYPTED, host-only browser cookie, is NOT
persisted server-side and cannot cross devices; build/deploy credentials resolve on the node (`git.rs`
`resolve_clone_credentials`).

## Placement & capacity

- Disk is a HARD filter (`HIVE_PLACEMENT_DISK_FLOOR_GB`), never a score term; unknown free space (`NodeInfo::disk_free_gb` 0) ADMITS;
placement requires an actually dialable path. **Every reclaim path needs a blast-radius guard**: `gc_rootfs_images` refuses an empty
keep-set or an orphaned fraction over `HIVE_GC_MAX_REAP_FRACTION` (data images carry a DOUBLE `dpl-dpl-` prefix: match both).

## Request path, circuits, cold start

- A deployment fails in two shapes: starts then dies (`crash_streak` + `last_warm_ok_ms`) or never listens
(`warm_fail_streak`); never clear a streak because a start succeeded. A broken deployment reports
`DEPLOYMENT_CIRCUIT_OPEN`, never `CAPACITY_EXHAUSTED`; the circuit is keyed on instance HEALTH (`unhealthy_reaps`),
never instance count.

## Managed data lanes

- Managed SQLite (`DbKind::Sqlite`) and `browser_db` share nothing but the word: a file per DATABASE at
`$HIVE_DATA/sqlite-dbs/hive-sqlite-{sanitize_tag(db_id)}.db` vs a cr-sqlite CRR per PROJECT at `$HIVE_DATA/browser-dbs/`. **Never
point the Hrana handler or musql at a `browser-dbs` file**: a bare `rusqlite` writer bypasses the clock tables the CRR merge reads.
- `browser_db`: grants ride the admission, server-derived and tenant-pinned, re-checked on EVERY request
(tenant+project+file derived server-side; `db_file` is an identifier, never a path); caps (`max_bytes`,
`max_value_bytes`) bind with a typed refusal + whole-batch rollback, never truncate/evict; bytes, site ids and
watermarks replicate ONLY through `hive_crsql` ChangeBatch (`Op::CrrSync`).
- A managed database is reachable ONLY through its `<slug>.{db_domain}` record from `host_node`'s address, with
`db_gateway` splicing on that SAME node, so a host that left `registry.nodes()` STRANDS it: `db_host_admissible` only
refuses NEW placement; stranded databases are reported, never re-hosted (re-provisioning yields an EMPTY database).
- `db_gateway` bounds a connection while UNPROVEN or DRAINING, never proven and idle: proven = the database ACCEPTED AUTHENTICATION
(`AuthProof`: Postgres `AuthenticationOk`, Redis `+OK`; `-NOAUTH`/`+RESET` prove nothing) within
`HIVE_DB_GATEWAY_ACCEPT_DEADLINE_MS` before the backend and `HIVE_DB_GATEWAY_AUTH_DEADLINE_MS` (30 s absolute) after; a full
`PendingTable` EVICTS the oldest stuck connection; `splice` copies each direction itself, never `copy_bidirectional` or a `select!`.

## Contract and execution transparency (managed-db)

- `crates/hive-contract`: a contract is DATA, not code. `Contract::declare` refuses `OpenParticipation` +
`StateWrite` at RUNTIME, not by type: public fields/`Deserialize` bypass it, so `validate`/`commit` MUST
recheck that pair (unmet: row contract-pair-deserialize-bypass). `crates/hive-cloud/src/db_contract.rs` is
the ONLY seam: it does NOT authorize or fail the request —
**the caller mutates FIRST and records afterwards**, so a REFUSED entry is a RECORD, not a prevention.
It counts writes (`record_write`) even on refusal. A `db.write` outcome is a MEASUREMENT,
not `Ok(())`; `created` forwards its outcome, `purged`/`deleted` pass `Ok(())`. An unattributed write is a
REFUSED `db.write` against `unattributed:<kind>:<tenant>::<name>`; unopenable log: NO entry; later
persist fault: `persist_error`. Docs: `docs/contract-platform-seam.md`.
- Canonical bytes are LENGTH-PREFIXED `<len>:<bytes>` (a bare join is NOT injective). `entry_sig_preimage`
folds `Ack::canonical` into the signed preimage (`entry_hash` = BLAKE3(OP_DOMAIN||prev||0x1c||op.canonical());
it cannot cover the ack, since `ack.log_head` IS that hash). No PINNED identity = Inadmissible, never Dishonest.
Checkpoint close normally defers to the drain; `submit` starts it, `wrote` awaits `submit`, not the
close. `Store::append` closes inline past 1 deferred close; rebuild repair closes inline. Read EVERY
segment a window spans.
- **Vitra has no consensus**: honesty by REPLAY (`replicate.rs`, `audit.rs`), not agreement. Inclusion and
append-only violation proofs exist; **NO bond or incentive model** (whitepaper §4.4 leaves it unspecified).
Proofs show an entry WAS published, not freshness.

## Account 2FA (optional per-account TOTP)

- The OTP gate sits INSIDE `admin::mint_token` before `auth::issue`; `auth::step_up_ok` re-checks per
request from BOTH `require_auth` and `admin_ingress` (`/v1/token` is `is_open_route`, exempt from
leader forwarding, `roles::gate` short-circuits for `platform_admin`/fleet). A RULES row,
handler-local check or node-local store is a bypass; new routes get viewer `roles::RULES` rows
mirrored in `ui/lib/team-role.ts`. Docs: `docs/account-2fa.md`.
- State is per verified `Claims.sub` in `store_sync::REGISTRY` + `MERGE_STORES` behind `serde(default)`.
Secret: `secrets::encrypt`
(FAILS OPEN, assert `is_encrypted`), read ONLY `secrets::try_decrypt`, compared `admin::ct_eq`. The
signed `otp_at` claim sits INSIDE the HS256 payload, read only from an already-verified token,
fails closed if absent. Carve out machines before any OTP demand: `is_fleet_service`,
`is_queue_client`, `key:`/`__untagged__`/empty subs, or all 20 fleet delegations and API keys break.
Dev nodes, GET/HEAD/OPTIONS never gated. Single-use challenge, `step > last_used_step`, per-sub rate
limit charged on wrong codes, Drop guard. Already-issued JWTs are NOT revoked; never rotate
`HIVE_JWT_SECRET` to invalidate them.

## Browser lane

- `fluid_core::browser_policy_digest` and `policyDigest` in `artifact-policy.js` are ONE contract with TWO
implementations (also `MESH_SIG_DOMAIN` / `peer-mesh.js`: drift silently rejects every handshake).

## DNS: Seer, cutovers/ACME, custom domains

- Seer answers the deploy zone affinity-first from `apps_host_owner`; Vercel DNS has no geo/health routing,
so Seer answers only delegated names and `desired_geo_delegation` refuses below TWO nameservers.
No code path writes an apex NS: the apex is NOT delegable. **A delegated name must never serve NEITHER
addresses NOR delegation**: Vercel refuses NS creation while any child record exists, so a cutover is a
restore-on-failure transaction in `vercel_dns::plan_writes` (creates before deletes, VERIFIED rollbacks).
- **Advertise only what peers have PROVEN**: `NodeInfo::dns_ns` needs `dns_bind_reaches_internet` (necessary,
never sufficient); `dns_probe` proves reachability and `validate_nameservers` admits a node attested from TWO distinct
REGIONS, never self-attested.
- A platform-wide public-DNS question (the `www` SAN, the `_hive-verify` TXT proof) is asked of `dns_quorum::VANTAGES` and admitted by
`Tally::admits`: a resolver that errors ABSTAINS (never a "no"), a MAJORITY of the ANSWERING operators is required with a floor of 2.
- Custom domains: routing keys are FULL hostnames (`fluid_gateway`'s `aliases_full`) before any first-label/wildcard lookup.
Attachment needs observed DNS proof through ONE helper (`mark_domain_verified`); only a VERIFIED record locks to its tenant;
detach/activate require `require_domain_owner_if_exists` AND a stored challenge whose project matches the path. Reserved
proof names (`_acme-challenge`, `_hive-verify`) are rejected in add/import/zone-parse.

## Security gates (never weaken)

- **`auth::require_auth` proves only "some valid token"** (a GET never fails on missing auth: `ANON_TENANT`, scoped PER
HANDLER); a ROLE is decided once by `roles::RULES` via `roles::gate` for every authenticated request on an enforced node,
reads included: viewer < member < admin < owner, a read is viewer, a write NOBODY LISTED is ADMIN, so a new route is admin
until listed. A handler never re-decides a role; `ui/lib/team-role.ts` mirrors `RULES` row for row. `owner` owns only the
caller's OWN tenant: a gate opens on a tenant match or `platform_admin`, never a role (`edge::claims_open_preview`).
- A `service` role STRING opens nothing: `auth::verify` accepts a `service` claim only for a FLEET identity
(`auth::is_fleet_service`: `mesh-internal` or `mesh-node:<coordinator>`, subject AND role); every other one is no principal.
A fleet-wide RPC is `require_mesh_rpc(.., Some("mesh"))`, never `None`. A `gossip::dispatch` handler is mounted publicly
ONLY through a gating wrapper (`fleet_deployments_public`; the inner handler authorizes the RESOURCE). A route with its own
credential joins `auth::is_open_route` (`admin_ingress` asks it too) AND fails closed itself.
- The queue token (`HIVE_QUEUE_TOKEN`) is the one tenant-held bearer: `auth::verify_queue_client` (subject AND role,
never `platform_admin`, `iat` within `HIVE_QUEUE_TOKEN_MAX_AGE_SECS`, 30 d), resolved by `auth::principal` (the ONE
resolution `require_auth`/`admin_ingress` share), confined by `roles::QUEUE_CLIENT_ROUTES` to enqueue + stats. It sits
in a tenant's env, so `world_queue::rotate_queue_tokens` re-issues an aged or sealed one in each local deployment's
function env 90 s after boot and every 6 h (`HIVE_QUEUE_TOKEN_ROTATE_SECS`), PLAINTEXT (an `enc:v1:` copy is no bearer).
- A project transfer is a MOVE: the settings row is the owner of record (`admin::row_holder`; copies are
evidence only for an untagged row), so a read keyed by a DEPLOYMENT ID asks ONE predicate,
`admin::deployment_owned_by`, never an inline copy or a project NAME/team SLUG versus a tenant
(`record_tenant` takes a stored tag). A claim never takes a project (`ProjectStore::claim_team` is a CAS);
deployments follow the row (`Gateway::retag_project`, then `tenancy_reconcile::retag_deployments_to_rows`
every host: `HIVE_DEPLOYMENT_RETAG_SECS` 15, `=0` disables). A move needs a destination the caller
administers (`caller_administers`) or the operator.
- Every roster change goes through `TeamStore::set_member_role`/`remove_member_as` (`check_roster_change`:
nobody changes, removes or grants above their own role; the last Owner stays), and `remove_checked` authorises
EVERY row a removal deletes (`check_roster_removal`), so a ghost row cannot carry the Owner away. A directory
sync only adds (`add_member_if_absent`) or removes a plain member (`remove_plain_member`).
- Only the dashboard SERVER writes the identity mirror (`identity_sync` under `mint_allowed`): it feeds `team_create` and
`caller_administers`, so a browser-originated sync records nothing.
- `POST /v1/token` checks the internal token FIRST: only a failing request spends the per-peer guess budget
(`mint_rate_limiter`, 20/min); a legitimate mint spends `HIVE_MINT_USER_PER_MIN` (30) and `HIVE_MINT_BACKSTOP_PER_MIN`
(600: the dashboard is one peer per user). A token carries its role for its whole life: `admin`/`member`/`viewer`
live `HIVE_ORG_TOKEN_TTL_SECS` (900 s), the rest 3600 s. Residual: ~20 colluding accounts (30/min each) exhaust it.
- **Scope at the store primitive, not the handler**: every `WebhookStore` operation takes a `Scope`/`Audience`; a queue
consumer targets only its own tenant's app (`queues::consumer_target_owned`, at registration AND every delivery); a
deployment reached through a project path is authorized by `storage_broker::authorize_deployment`; an API key's role
caps at its minter's (`apikeys::check_new_role`). Peers answer `?local=true` UNFILTERED, so a tenant-facing fleet
aggregation re-scopes merged peer rows (`admin::non_operator_node_view`).
- **Every tenant-influenced URL goes through `hive-outbound`**, never a bare `reqwest::Client`: `validate_url` (https only,
no userinfo, no internal/non-public address) at registration AND before every request; `client()`/`following_client()`
refuse non-public addresses at connect (no proxy; redirects re-validated per hop); `same_origin_client` for calls INTO a
tenant app, `internal_client` (no redirect) for peers/platform URLs. A tenant-facing failure is one of
`hive_outbound::describe`'s fixed sentences; `HIVE_OUTBOUND_ALLOW_PRIVATE=1` is a developer-only escape, REFUSED when
`HIVE_JWT_SECRET` is set.
- A caller-controlled string never reaches a URL, HTML or a `Location` by interpolation: parse a trusted base, then
`Url::set_host`/`set_path` (`state::host_scoped_https_url`, `edge::with_request_target`; `//host` is an authority, so `Url::join`
needs a same-origin check). `edge::password_page` has NO form `action`, shown text passes `edge::html_escape`, a redirect passes
`auth::same_origin_path`.
- A tenant never names a loopback address: `world::world_config` accepts one only when it equals the REST URL of one of the project's
own Redis records on this node (`DatabaseStore::local_redis_rest_urls`).
- A plan is bought or granted, never self-assigned: `admin::apply_plan_everywhere(.., PlanChange)` refuses a raise
without `Paid` or an operator; a REQUESTED plan resolves via `billing::known_plan` (never `plan_spec`'s Hobby fallback);
`BillingStore::confirm_checkout(id, Payment)` completes an unproven payment only where `billing::mock_checkout_allowed()`
and a Stripe one only when `billing::session_paid`; every cents input is bounded by `billing::MAX_AMOUNT_CENTS` (a `u64
as i64` goes NEGATIVE).
- SAML ACS verifies no assertion signature, so only the path team's owner/admin may post it (`require_team_floor`); SCIM is open at the
platform gate, managed by an owner/admin and authenticated ONLY by its per-team bearer (`scim_team_for_token` fails closed);
`zkauth::bootstrap` binds the proof's ring to the project's owner (`local_owning_team`).
- A route that execs tenant commands checks the ACTIVE backend (`/v1/sandbox` refuses on Mock, which runs on the HOST shell), and its
per-tenant cap is held by the submitted JOB, never the request's Drop guard.
- A cookie, token or stored password that DECIDES access is read with `secrets::try_decrypt`, never the passthrough `decrypt`; the
password-gate unlock cookie is a server-sealed `v1.<expiry ms>.<HMAC>` over (deployment, expiry, stored hash)
(`EnterpriseStore::seal_password_cookie`, `HIVE_PW_COOKIE_TTL_SECS`), `Secure` unless plain-http loopback
(`edge::unlock_cookie_header`).
- A destructive op needs an ASSERTED identity before any tenant is resolved (`has_explicit_caller_identity`: an unenforced node's
implicit "personal" would own everything); every secret or MAC comparison is constant-time (`admin::ct_eq`; `==` is for ids and
digests).
- **A tenant string never becomes a host path.** Podman reads `-v <a>:<b>` as a bind mount whenever `<a>` looks like a path, so the left
side is always a platform-templated NAME (`container_volume_cfg`); same for DB file names, artifact digests (64 lowercase hex, else
400) and snapshot dirs.
- **Tenancy comes from the cryptographically-bound JWT claim, never the spoofable `x-hive-team` header**; an untagged tenant is a
fail-closed sentinel (`UNTAGGED_TENANT`), never collapsed to "personal". Gossip arms read the tenant with `mesh_tenant()`, never a raw
`split_once`.
- Tenant processes get their env from nothing (`env_clear()` in every backend: the node env holds `HIVE_SECRET_KEY`,
`HIVE_JWT_SECRET`, `HIVE_INTERNAL_TOKEN`, and mock-backend nodes serve real tenants). The proxy re-derives `x-forwarded-host`/`-proto`;
the edge's client IP is the real TCP peer, never `x-forwarded-for`.
- `HIVE_SECRET_KEY` is the fleet-shared at-rest key: never rotate it without the previous key in
`HIVE_SECRET_KEY_OLD` (comma-separated hex); `decrypt` returns its input unchanged on AEAD failure,
`try_decrypt` is the honest `Option`. Key resolution fails CLOSED (`secrets::load_or_create_key`,
`IntegritySigner::open_or_create`): a bad/unpersistable key file refuses to boot, never regenerates;
`scripts/audit-secret-keys.sh` is the PRE-ROLL check. Never print a plist, manifest or `/proc/*/environ`
wholesale: they carry this key. `put_env` force-masks credential-shaped values (`looks_like_secret`).
- PQ transport: the QUIC transport offers hybrid X25519MLKEM768 first via an explicit `.crypto_provider(...)` in
`hive_p2p::bind_full` (report the REAL group, `pq_kex_stats`); **transport IDENTITY stays classical ed25519.**

## Hashing: content identity vs. security binding

**BLAKE3 mints every NEW content identity** (`fluid_core::content_hash`, `ContentHasher`); `verify_content` accepts a
stored digest under BLAKE3 or legacy hex(SHA-256) over the same domain and alone decides which it came from.
Pre-domain-separation digests are bare (`raw_content_hash`/`verify_raw_content`). A digest that NAMES stored bytes keeps
its writer until `content_verify_stats()` reports `accepted_sha256` flat across a deploy cycle (`content_sha256` IS the
store dir name).

**SECURITY BINDINGS, never touch the algorithm**: `fold_integrity_chain` signed by `IntegritySigner::sign_chain_head`
(ed25519 stays classical); every HMAC-SHA256 (`hmac_sha256`, `verify_github_sig`, `verify_webhook_signature`,
`sync_one_peer` join proof, `consumer_signing_secret`, `tc3_request`, `ticket_key`); credential hashes (`sha256_hex` in
`apikeys.rs`/`browser_db_rest.rs`/`drive_api.rs`/`world.rs`, Argon2id `password_hash` with the legacy
`sha256(project ":" password)` branch kept, `derive_secret`, `hive-zkauth`, `custom_bundle_name`); handshake/supply chain
(`handshake_transcript_sha256`, `sha256_file` for runsc, `image_digest`: literal OCI `sha256:`).

@.gm/next-step.md
