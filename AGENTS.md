# AGENTS.md

Working conventions for this repo, present tense — what must be true of the code
now. History lives in `CHANGELOG.md`.

**Self-maintenance:** this file stays under ~30 KB. When it grows past that,
recompile it from the code, the git log and the memory files — keep only what is
still true and load-bearing, drop every incident narrative, and delete the
memories/notes whose durable content it absorbed.

## Process & verification

- **No test files, ever** — no `test/`, `__tests__/`, `spec/` directories, no new
  `#[cfg(test)]` modules to verify a change, no assertion/mocking libraries.
  Verify with `cargo test --workspace` plus real live execution.
- `cargo check` skips `cfg(target_os = "linux")` code AND test targets, so a green
  local (macOS) check proves nothing, and the fleet is all Linux. Run
  `cargo test --workspace --no-run` first; `0`/`None` on `disk_free_gb`/
  `gpu_free_mb` mean UNKNOWN.
- Two glibc build groups: **2.38** = bkk, hk, all five GPU/CVM nodes; **2.39** =
  va/va2/va3/sj/sj2/sp/fr. By OS image, never region; sha256-verify and keep a
  `.old` backup before swapping.
- Backend (`hive-cloud`, `hive-node`) and dashboard (`ui/`, `hive-ui`) deploy
  independently — a backend-only roll does NOT ship a `ui/` change; use
  `scripts/deploy-ui-fleet.sh`, verified against the real public domain.
- Git only through the `gm` skill's git verbs; never raw `git`. zsh `path`,
  `status`, `options` are special/read-only.
- Binary swap: stop → pkill the old PID → verify sha256 → fresh-inode write
  (`cp` then `mv`) → `reset-failed` → start. A bare `mv` + restart crash-loops
  ETXTBSY. `firecracker`/`crun`/`runsc` drift per node — audit on every CVE and
  verify the advisory's affected RANGE before upgrading.

## Control-plane ownership & leader-only jobs

- With `HIVE_CP_OWNER_CHAIN` set, the owner is the first chain entry PRESENT in
  `registry.nodes()` with a `peer_id` and a public address — never the observer's
  `healthy` flag, never the identity election (`Cluster::strict_chain_owner`). No
  qualifying entry = HOLD and forwarded mutations answer the retryable 503.
- `control_plane_leader()` never falls back to this node; every read proxy and
  owner-forward asks ONE question, `CloudState::leader_forward_target()`.
- **Every leader-only background job asks exactly one question:
  `leadership::may_act(cloud, Job)`** — owner AND a fresh view AND not isolated
  AND, for a BACKUP owner only, fresh DIRECT contact with a strict majority of
  `HIVE_CP_VOTERS`, held continuously for `Job::min_tenure()` (relocate 600s,
  ACME/HTTP-01 300s, billing 120s, DNS and git poll 60s, others 30s). A new
  leader-only loop adds a `Job` variant; the chain HEAD acts on presence alone.
- Take-over work keys on the ownership TERM (`leadership::TERM_LAPSE`, 30s), not a
  `may_act` edge. An owner present but unable to act is still owner in everyone's
  view, so every job HOLDS fleet-wide — fix by restoring transport or voters,
  never by weakening the gate. Verify `HIVE_CP_OWNER_CHAIN`/`HIVE_CP_VOTERS`
  **in the running process** (`/proc/<pid>/environ`, or `launchctl print` on
  macOS), since a drop-in or plist overrides the unit file.
- Node-death self-heal is OFF unless `HIVE_NODE_DEATH_SELF_HEAL=1`. Enabled it
  needs `may_act(NodeDeathRelocate)`, the host absent from `registry.nodes()` for
  10 min measured from this node's first observation (never the row's
  `updated_ms`), no retry for the same (project, host, commit) in 6h, and <3
  relocations from this node in the trailing hour.
- **Automated incidents are level-triggered conditions** (`incidents.rs`): the
  opener calls `IncidentStore::open` EVERY pass with a stable `condition` and a
  `ttl_ms` of 3 passes (dedup + re-assert), `clear(condition)` on its healthy
  branch; the leader-only reconciler (`Job::IncidentReconcile`) resolves what
  lapsed, collapses duplicates and bounds history. Only the operator's
  `POST /v1/incidents` uses `open_new`. `GET /v1/incidents` is paged + gzip.

## State: replication, routing, relational mirror

- `admin_ingress` forwards MUTATIONS to the leader and serves every GET/HEAD
  LOCALLY; public hosts are round-robin DNS. Any endpoint on NODE-LOCAL data (a
  file under `persist::data_dir()`, an in-process `Mutex`/`OnceLock`/channel) that
  a mutation writes and a GET reads will read the wrong node and return
  empty/404/0 silently. Every new endpoint declares its side: a GET on node-local
  state needs an owner/leader proxy (`build_get`, `fetch_from_host`,
  `fetch_bytes_from_host`, `sandboxes_api::proxy_to_owner`) plus a
  `gossip::dispatch` arm ordered longest-prefix-first, or its state moves into
  `store_sync::REGISTRY` or fans out (`db_replicate::fanout_all` + a matching
  `apply_mirrored_write` arm). Verify through the round-robin host with several
  reads.
- The stale-epoch 503 is a retryable refusal: the fence answers 503 plus
  `x-hive-cp-epoch-current`; the forwarder max-merges it via `adopt_epoch`,
  re-stamps, retries up to `MAX_STALE_EPOCH_RETRIES` (3), then walks candidates.
- **Store-sync adoption is WHOLESALE-REPLACE, so it happens only from an owner
  every node agrees on** (`OwnerSource::may_adopt_wholesale_from`): the chain HEAD,
  or on a chain-less mesh the identity election; from a backup chain owner or a
  `Fallback` guess only per-row `MERGE_STORES` are pulled. A failed pull is never
  silent (`StorePullFailures` WARNs per store, incident every 10 failures), and the
  pull is `store_follower` — its own supervised loops, never behind a GuardianDB
  await. A critical write must land on EVERY owner-chain candidate, and snapshots
  must route through `serde_json::Value` with `enc_sorted()`.
- **The relational index is built by ONE background walker, never on a request
  path, and nothing writes the catalog before it completes once.** An EMPTY
  guardian index makes every `Session` load `Catalog::new()` (every table "does not
  exist", `pg_class`=0) and a `CREATE TABLE IF NOT EXISTS` then overwrites the real
  catalog. `spawn_index_refresher` walks once under
  `HIVE_RELATIONAL_INDEX_BUILD_SECS` (900), re-walks every
  `HIVE_RELATIONAL_REFRESH_SECS` (120); `index_ready()` gates `init_schema` and
  `ensure_table_exists`, which verifies through `information_schema.tables`, never
  `SELECT 1 FROM t`. `get`/`scan` fall through to the store's ASYNC `get`.
  Diagnose with `SELECT count(*) FROM pg_catalog.pg_class` (0 = unbuilt).
- `GET/POST /v1/admin/sql/*` is read-only by construction
  (`reject_unless_readonly` scans the whole query); `known_tables()` (15 today) is
  hand-maintained, and `relational::upsert_billing` wraps a tenant's whole write in
  ONE `BEGIN; … COMMIT;`.
- **GuardianDB and its relational SQL mirror must never be an input, a translation
  layer, or a readiness gate for canonical state — at most a derived read-only
  projection.** Fix a failure CLASS once at a shared primitive, never per call
  site. Cron is NODE-LOCAL — keep it out of store_sync and leader-only execution,
  and restore it with `CronScheduler::replace_all`, never `add()` in a loop.

## Mesh transport, discovery, watchdogs

- `PeerPool` keys trunks by ENDPOINT ID, never the caller's label.
- **Every inbound accept is bounded, budgeted by its NEGOTIATED ALPN, and never
  evicts an established trunk.** Each accept runs under
  `HIVE_P2P_ACCEPT_DEADLINE_MS` (default `IDLE_TIMEOUT`+10s = 40s, above the 30s
  idle timeout so an abandoned handshake ends as an error, uncounted); past it the
  connection is CLOSED with `FLEET_CLOSE_OVERLOADED` (8) and counted
  `accept_stuck`. Pre-handshake the ClientHello picks the budget (browser 128 /
  fleet 512 / unparseable → `pending`, `HIVE_P2P_PENDING_MAX_CONNS` 256); after the
  handshake `conn.alpn()` decides. Every hybrid X25519MLKEM768 dial lands in
  `pending` (its 1216-byte key share does not fit one Initial) — never charge an
  unparsed ClientHello to the fleet budget. Connections are also capped per remote
  endpoint (`HIVE_P2P_MAX_CONNS_PER_ENDPOINT`, 16).
- A refused dial opens a per-endpoint window (1s→30s): `acquire` fails at once with
  `PeerRefused`, never `DeadPeerTimeout`. A pre-handshake refusal names no
  identity, so one through a hint's DIRECT addresses opens the window only once an
  identity-routed path confirms it; `UnknownIssuer` suspends EVERY direct address
  of that hint for 10 min and retries via the relay — only while the hint HAS one.
  Never let an address filter empty the set. Gossip bodies read against IDLE
  (`HIVE_P2P_FIRSTBYTE_MS` 15s, `HIVE_P2P_IDLE_MS` 45s); replies ≥256 KiB are
  lower priority.
- `hive_p2p::establish_stats` is served on operator-only `GET /v1/mesh/establish`,
  **never on the unauthenticated `/v1/mesh`**. `last_outbound_established_ms`
  frozen while dials time out and warm trunks still gossip IS the wedge;
  `accept_stuck` alone is not evidence.
- Keep the vendored iroh "read before send" patch across pin bumps (`vendor/` is
  synced by the fanout role — edit vendored crates only between rolls);
  `Dropping received relay packet: no available capacity` on a NON-leader is news.
  A failed rebind must be RETRIED (`PendingRebind`): `AddrInUse` otherwise leaves
  the transport Closed.
- Address lookup: every source except the public DHT presupposes reachability, so
  `hive_p2p::dht` stays registered and every failure path leaves it UNREGISTERED
  with a WARN, never failing `bind()`. Never hand `DhtBuilder` a hostname. DHT
  records are public forever, default relay-only (`HIVE_DHT_PUBLISH_DIRECT=1`),
  RFC1918/CGNAT/link-local stripped — filter on the DHT builder, never
  `Endpoint::builder().addr_filter()`.
- A relay hint must name the relay the peer is ATTACHED to: keep the relay in the
  peer's gossiped `iroh_addr`, steering via `hive_edge::select_relay_hint` only
  when the addr has none. `NodeInfo::relay_url` (`http://<public-ip>:3341`) is
  TCP-dead across Tencent hosts; `https://*.relay.shadw.app:3343` answers from
  every vantage.
- **A gossip round never waits on a dead peer, and one stale reading never
  withdraws one.** Rounds end at `HIVE_GOSSIP_ROUND_DEADLINE_MS` (8000) using ONE
  fan-out primitive, `bounded_round::BoundedRound`, never a hand-rolled
  `join_all`. The deadline alone let ONE slow target pin every round to the full
  8s, so a target that has missed it `LEASH_AFTER_ROUNDS` (2) times running is
  waited on only `HIVE_GOSSIP_STRAGGLER_LEASH_MS` (2500) — still dialed every
  round, and one answer inside the round restores the full deadline. A leash
  PARKS, never cancels, so it never cuts a dial short mid-discovery; leash
  state is keyed by ENDPOINT id, never target label. A slow sync is LATE,
  never absent (`round_contributions`); backoff
  applies only while this node's view is fresh; `health::demote` withdraws only
  after `DEMOTE_STALE_ROUNDS` (2) stale rounds and HOLDS while the loop is stalled.
- meshwatch: total isolation (600s), cumulative degradation, and
  `establishment_wedge` — which reads transport counters, never peer counts: no
  fresh connection in either direction for `HIVE_MESH_ESTABLISH_WEDGE_SECS` (300)
  while dials to ≥2 distinct peers TIMED OUT, or ≥3 accepts closed at the deadline
  with zero successful accepts. A refusal does not count; stuck accepts alone only
  WARN.
- **Every automatic restart passes ONE chokepoint, `ControlledRestart::request` →
  `RestartReason::admissible`**: refused until `hive_backend::orphan_reap_ran()`
  confirms this boot's cell reap (false on macOS and with
  `HIVE_CELL_ORPHAN_REAP=0`, so those nodes only WARN) and rate-capped per reason
  (wedge 1 per 6h), with the reason stamped into the run marker so the cap counted
  from `restart_history.json` fails CLOSED when it is unreadable.

## Node bring-up, health, host firewall

- Seed `HIVE_BOOTSTRAP_PEERS` with ADDRESSED peers
  (`<64hex>[@ip:port[+ip:port]][|relay-url]`), never bare ids; `peer_iroh.json`
  holds peers' PRIVATE addrs and cannot seed another node. A node's trust list
  (`HIVE_TRUSTED_NODE_IDS` + `HIVE_PEER_TRUST=1`) must list every fleet id, and so
  must every existing node's. Join rides a dedicated `STREAM_JOIN` mode
  authenticated by the QUIC remote id plus an HMAC over `HIVE_JWT_SECRET`.
- A health verdict is PER-OBSERVER, and the verdict that decides traffic is the
  leader's. A successful cross-continent probe measured 7462ms; never trust a TCP
  probe from a laptop on the VPN (it SYN-proxies).
- **The published-port range (TCP+UDP 20000–29999) is SG-open to the internet on
  every platform host** — never run an ad-hoc file server on a fleet node.
  `scripts/audit-public-listeners.sh` is the check (exits 1 on a finding).
- `scripts/hive-lockdown.sh` is the fleet's only host firewall and its PEERS roster
  is generated (`scripts/gen-hive-lockdown.sh` then
  `ansible-playbook playbooks/site.yml --tags lockdown`), never typed. Do not gate
  it on the listener's cgroup.

## Containers, podman locks, process lifecycle

- **`KillMode=process` on hive-node.service is load-bearing.** Rootful podman's
  conmon inherits the caller's cgroup, so the default KillMode SIGTERMs every
  managed database and Supabase stack on each roll; `OOMPolicy=continue` likewise.
- The price is that tenant CELLS outlive their process, so every boot reaps its own
  previous boot's cells. `hive_backend::reap_orphaned_cells` runs right after
  backend selection; boot waits only for its URGENT half (listing + SIGKILL of
  duplicate writers, ≤20s). **Ownership is owner + boot, never boot alone**
  (`hive.owner`, `hive.boot`): another owner's cell is never touched; label-less
  legacy cells only where this process is the host's sole supervised instance or
  under `HIVE_CELL_ORPHAN_REAP_LEGACY=1`. With ≥2 live writers on a volume every
  stale one is SIGKILLed (a SIGTERM makes each duplicate flush older state) except,
  when no current or foreign writer shares it, the NEWEST, which gets
  `podman stop -t 10`; removal is always `rm -f -v`. `orphan_reap_ran()` is false
  while the reap runs, when skipped, when a stale cell survived, when opted out,
  and ALWAYS on macOS.
- Graceful stop must finish inside systemd's timeout: a `persist()` arriving after
  `flush_blocking` closed admission is REFUSED, never blocked (a condvar wait there
  parks a tokio worker and every timer with it);
  `HIVE_SHUTDOWN_DEADLINE_SECS` (75) lives on a std thread; `TimeoutStopSec=90s`
  is explicit (TencentOS defaults it to 5s). Any path that removes a container must
  pass `-v` (`container_cli::rm_args`), since podman allocates one lock from a
  **fixed pool** (2048) per CONTAINER and per VOLUME — leaked locks starve the
  whole node and surface as 503 `CAPACITY_EXHAUSTED`. **Never `podman volume
  prune`** — reclaim is gated on `is_anonymous_volume` (exactly 64 ascii-hex) AND
  `dangling=true`.
- macOS launchd nodes: recall `macos-launchd-fleet-gotchas`.

## Isolation backends & capability gating

- **Capability is PROBED and ADVERTISED, never assumed**, on the filesystem the
  cell execs against, re-probed on the disk-refresh tick: wasmer on
  `NodeInfo::wasm_runtime` (`resources::detect_wasm_runtime`); bun on
  `NodeInfo::bun_runtime` (litebox always `Some(false)`); the build toolchain
  (probe script inside the builder image for firecracker, host PATH otherwise).
  `None` means NOT CAPABLE, unlike `disk_free_gb == 0`. Placement gates through ONE
  predicate shared by `place`, lease-stickiness and `dispatch_fallbacks`
  (`schedule::wasm_capable`, `bun_capable`, `build_isolation_capable`); a missing
  runtime is a NODE fault (`fault::NODE_RUNTIME_MISSING`).
- BUILD-ENV bun and GUEST/RUNTIME bun are independent: a bun PACKAGE-MANAGER choice
  must NOT gate on `bun_runtime`, and a package manager the manifest selects but
  the node lacks is substituted (npm first) with a loud log. Exit 127/42 of an
  explicit repository command maps to typed `BUILD_TOOLCHAIN_MISSING`/`MISMATCH`.
- PVM kernels: `pti=off` is REQUIRED and `KVM_CREATE_VM` succeeding does NOT mean a
  microVM boots (can hard-reset the host): recall `pvm-kernel-hard-facts`.
- Litebox (third `CellBackend`, Firecracker → Litebox → Mock): a node is Litebox
  only after `hive-cloud --litebox-probe` PASSES on that host with the exact staged
  runner AND `litebox_verified=true` on its inventory line — never the flag alone,
  never on macOS. The guest tree is staged via `--initial-files=<tar>` with
  `tar -h`. Honest posture: seccomp-bpf beats Mock, but guest and enforcement
  share one address space and JIT syscalls are an unclosed gap. Guest rules
  (fork-free execve, one TUN owner, exec drains under a deadline): recall
  `litebox-runtime-contract`.

## Runtime discipline (serving runtime never stalls)

- **Heavy work never runs on a serving tokio worker.** The guardian SQL engine
  reloads every row of each table it touches PER STATEMENT, so
  `relational::exec` (the one chokepoint) runs under `block_in_place`; the
  mirror loop and index walker live on the `bulkhead` runtime
  (`bulkhead::spawn`, `HIVE_BULKHEAD_THREADS` 2); billing mirrors send only
  this tick's ledger rows as chunked multi-row INSERTs (`ROWS_PER_INSERT`
  500), full re-send every 60 ticks; two workers at 94% CPU there froze the
  leader 73–763 s. A loop whose cost grows with the data is
  spawned on the bulkhead and bounded. `runtime_watch` probes the runtime
  every 1 s from an OS thread and names the CPU-hot threads of any stall >2 s
  (`/v1/admin/runtime`; `HIVE_RUNTIME_STALL_DUMP=1` adds a gdb stack); release
  keeps the symbol table (`strip = "debuginfo"`). Keep `MemoryHigh` above the
  charged page cache (leader 112G/128G): memory.high throttles every thread.

## Builds & deploys

- `git push` auto-deploys through TWO triggers, both deduping on the commit SHA
  (`CloudState::git_poll_seen`): the webhook and `git::spawn_git_poll_reconcile`.
- **A single unreachable node never fails a deploy.** On
  `FanoutOutcome::nothing_ran()` `run_build` walks
  `schedule::dispatch_fallbacks` one candidate at a time with the same `capable` +
  `reachable` predicates as `place` (never the region widening), bounded by
  `HIVE_DEPLOY_DISPATCH_FALLBACK_MAX` (3), stopping when any node RAN it.
  `node_admins` holds http(s):// URLs only.
- Isolated BuildExecutor (`build_executor.rs`): recover `HIVE_BUILDAH_SOCKET` from
  the tenant process's `/proc/<pid>/environ`; never flip `build_isolation_protocol`
  (main.rs hard-codes `None`). Full gotcha list: recall `build-executor-isolation-gotchas`.
- Checkouts live in the DURABLE root `git::deploy_root()` (`$HIVE_DATA/deploys`);
  prefix scanners must cover BOTH roots and BOTH name forms via
  `git::checkout_prefixes`. ZIP extraction REJECTS symlinks and directory entries.
  A direct-entry function whose entry does not exist fails the BUILD, never the
  launch and never as a node fault (`git::preflight_direct_entries`).
- The relocation reaper is SCOPED: `cleanup_non_targets` runs only for builds
  provably classified PRODUCTION and removes ONLY superseded production-lane
  records via `/v1/projects/<p>/reap-deployments`. ProjectSettings rows replicate
  with per-row `updated_ms` + permanent tombstones; merges never let absence erase
  a row. A preview's URL is `commit_alias || branch_alias || id_alias`, and the
  ledger's checksum is verified over the payload bytes AS STORED
  (`Box<RawValue>`), never over a re-serialization.

## Placement & capacity

- Disk is a HARD filter, not a score term (`HIVE_PLACEMENT_DISK_FLOOR_GB`, set
  above the per-cold-start floor). `NodeInfo::disk_free_gb` is gossiped and
  refreshed on a timer; unknown (`0`) must ADMIT.
- GPU: container path only; eligible only on `NodeInfo::gpu_count > 0` nodes,
  including the lease-stickiness path, with no silent CPU fallback. Free VRAM
  comes from the driver (`gpu_free_mb`) and the pool takes the MINIMUM of measured
  and estimated. No `--split-mode tensor` on T4; cross-node pooling is `--rpc`.
  Managed inference elects its coordinator deterministically (FNV(project)); the
  app reads `HIVE_INFERENCE_URL`. llama.cpp stays on CUDA 12.x.
- Data images carry a DOUBLE prefix (`dpl-dpl-<hash>.data.ext4`), so a GC keep-set
  built from raw deployment ids matches nothing. `gc_rootfs_images` matches both
  forms and refuses an empty keep-set or an orphaned fraction over
  `HIVE_GC_MAX_REAP_FRACTION`. **Every reclaim path needs the same blast-radius
  guard** — the same shape guards `browser_artifacts::gc`, `browser_db` GC and the
  guardian reaper (which also refuses when `HIVE_NODE_ROSTER` is unset).

## Request-path failure invariants

- A deployment fails in two shapes needing separate handling: starts then dies
  (`crash_streak` + `last_warm_ok_ms`) or never listens (`warm_fail_streak`).
  Never clear a streak merely because a start succeeded; both the autoscaler AND
  the request path must record failures.
- A broken deployment reports `DEPLOYMENT_CIRCUIT_OPEN`, never
  `CAPACITY_EXHAUSTED`. A circuit's open window (`CIRCUIT_PROBE_INTERVAL_MS`) must
  outlast the failure it guards.
- **Anything a request path reserves must be released by a `Drop` guard, never
  only on the `Err` branch** — axum DROPS the request future when a client gives up
  and a dropped future never returns `Err`. `ColdStartGuard` is the pattern; it
  applies to `PooledConn`, litebox guest tars/links and sandbox exec drains. Count
  failure streaks at the ONE chokepoint every caller funnels through.
- Tenant tier lives in BOTH `c.teams` and `c.billing`; neither is authoritative
  alone, so **every tier change goes through `admin::apply_plan_everywhere`** and
  deletion clears both (`team_delete`, keeping the ledger).

## Managed data lanes

- **Managed SQLite (`DbKind::Sqlite`) and `browser_db` share nothing but the
  word** — a plain file per DATABASE at
  `$HIVE_DATA/sqlite-dbs/hive-sqlite-{sanitize_tag(db_id)}.db` vs a cr-sqlite CRR
  replica per PROJECT at
  `$HIVE_DATA/browser-dbs/hive-browserdb-{sanitize_tag(project)}.db`. **Never point
  the Hrana handler at a `browser-dbs` file** — a bare `rusqlite` writer
  (`sqlite_pool`) bypasses the clock tables the CRR merge reads, i.e. silent
  permanent divergence; `browser_db_rest` is CRR-safe only because it opens via
  `hive_crsql::open` and applies `set_ts`. The SQLite lane is owner-routed, never
  leader-routed: the owner re-checks the bearer and refuses to re-proxy, so an
  unreachable owner is an honest 421.
- SQLite (Hrana) wire contract is strict (strings for integers, padded base64 blobs,
  v2+v3 never v3-protobuf, DSN ends in `/`): recall `sqlite-hrana-wire-contract`.
- `browser_db`: grants ride the admission, server-derived and tenant-pinned (Public
  scope read-only, only with `public_read`); caps bind BOTH sides (`max_bytes`
  64 MiB, `max_value_bytes` 1 MiB) and over-cap is a typed refusal + whole-batch
  rollback — never truncate or evict. Bytes/site-ids/watermarks replicate ONLY
  through `hive_crsql` ChangeBatch (`Op::CrrSync`), and the fleet re-checks the
  grant on EVERY request against its own admission view, deriving
  tenant+project+file server-side, so `db_file` is an identifier, never a path.
  cr-sqlite v0.17 does not replicate schema, so both halves derive it from the
  spec's DDL + `crsql_as_crr`. `HIVE_CRSQL_EXTENSION_PATH` must point at the
  packaged `/var/lib/hive/crsqlite.so`.

## Browser lane

- `fluid_core::browser_policy_digest` and `policyDigest` in
  `crates/hive-browser/www/function-runtime.js` are ONE contract with TWO
  implementations — drift silently breaks every artifact pin. Same for
  `val_payload_bytes` (hive-crsql and `hcb1.js`). A browser function is eligible
  only by explicit fluid.json opt-in (which FAILS THE BUILD when ineligible) or
  build-time auto-detection; ESM syntax is REWRITTEN to CommonJS
  (`browser_esm.rs`).
- Bytes stay node-local (`$HIVE_DATA/browser-artifacts/<policy_digest>.js`); only
  descriptor metadata replicates, delivery re-verifies size and source BLAKE3
  before serving (404 for a foreign tenant), and admission capabilities are
  entirely server-derived — `AdmissionRequest.digest` is accepted but never read,
  and `trusted_callers` comes from the live HEALTHY registry.
- COOP/COEP: the default is `HIVE_COI=lane` (global `require-corp` breaks the
  dashboard's Clerk embed and tenants hotlinking CORP-less assets) and
  `HIVE_COI_COEP=credentialless` (Safari implements neither value). `coi::layer`
  on the public listener is the ONLY writer. Isolation is DETECTED
  (`crossOriginIsolated`) and its absence is REPORTED, never waited out.

## DNS: Seer, cutovers/ACME, custom domains

- The delegation boundary is the whole story: Vercel DNS has no geo or health
  routing, so Seer can only answer for names delegated to it, and
  `desired_geo_delegation` refuses to publish below TWO nameservers. **A delegated
  name must never serve NEITHER addresses NOR delegation** — Vercel refuses NS
  creation while any child record exists, so every cutover runs as a
  restore-on-failure transaction in `vercel_dns::plan_writes` (creates before
  deletes) with VERIFIED rollbacks, and `ReconcileGuards`' create circuit skips
  delete-first steps while creates are broken. `VercelApi::list()` MUST paginate
  (100/page, `until` cursor).
- **Advertise only what peers have PROVEN**: `NodeInfo::dns_ns` is necessary and
  never sufficient, so `dns_probe` (every node) proves reachability and
  `validate_nameservers` admits only a node attested from TWO distinct REGIONS,
  never self-attesting. Below two proven nameservers the reconciler HOLDS the
  published NS set rather than withdrawing and opens an incident. `publishable()`
  is damped both ways; the ACME orphan sweeper deletes unknown `_acme-challenge.*`
  TXTs older than 15 min (unknown age means KEEP).
  **Gotcha:** `dns_ns`/`dns_api` derive from `HIVE_DNS_ADDR` ONLY when its host is
  a wildcard. **Gotcha:** on Tencent CVMs the "public" IP is on NO interface;
  where aardvark-dns needs `:53`, bind the PRIVATE IP.
- Geo tailoring locates by EDNS Client Subnet else source address; the primary
  source is the LOCAL committed table (`crates/hive-cloud/assets/geoloc.bin`) so no
  third party sees a client prefix, and `HIVE_DNS_GEO_ENDPOINT` is the only remote
  call and must stay optional. A tailored answer carries a non-zero ECS scope;
  health beats proximity. Seer answers are bounded (`MAX_RECORD_VALUE_BYTES` 4096,
  `MAX_RECORDS_PER_NAME_KIND` 32; UDP >1232 bytes truncated with TC=1).
- Custom domains: routing keys are FULL hostnames (`fluid_gateway`'s
  `aliases_full`), checked before any first-label/wildcard lookup. Attachment is
  gated on observed DNS proof — the activating node itself must observe the
  `_hive-verify.<domain>` TXT via DoH — and every verified-mark goes through ONE
  helper (`mark_domain_verified`, which also pins the delegated zone's system apex
  A/AAAA). Only a VERIFIED record locks to its tenant; detach and activate both
  require `require_domain_owner_if_exists` AND that the stored challenge's project
  matches the path project. TLS is HTTP-01 with apex+`www` SANs only (LE never
  offers HTTP-01 for a wildcard), and the port-80 listener answers
  `/.well-known/acme-challenge/` from the replicated `Http01Store`, proxying a
  miss to the leader (LE fetches over plain HTTP within ~1s from random nodes).

## Security gates (never weaken)

- **A tenant string never becomes a host path.** Podman reads `-v <a>:<b>` as a
  bind mount whenever `<a>` looks like a path, so the left side is always a
  platform-templated NAME: `container_volume_cfg` builds
  `hive-vol-{sanitize_tag(project)}`, sanitizing to `[a-z0-9._-]` and prepending
  the prefix AFTER sanitization. Same rule for DB file names, browser artifact
  digests (64 lowercase hex, else 400) and snapshot dirs (`sanitize_tag` passes `.`
  through, so the id is embedded inside a `snaps-<id>` component, never bare). Any
  future storage feature must preserve this.
- **Tenancy comes from the cryptographically-bound JWT claim, never the spoofable
  `x-hive-team` header**; an untagged tenant is a fail-closed sentinel
  (`__untagged__`/`UNTAGGED_TENANT`), never silently collapsed to "personal".
  Gossip arms carrying a team token must parse it with `team_claims()`/`qparam()` —
  a raw `split_once("?team=")` swallows the token and every ownership compare
  silently misses. API-key-derived claims always have `platform_admin: false`.
- `HIVE_SECRET_KEY` is the fleet-shared at-rest key: **never introduce or rotate it
  without carrying the previous key in `HIVE_SECRET_KEY_OLD`** (comma-separated
  hex) — `decrypt` returns its input unchanged on AEAD failure, so an orphaned
  value silently hands callers raw `enc:v1:` ciphertext. `try_decrypt` is the
  honest `Option`-returning variant.
- `ProjectStore::put_env` force-masks credential-shaped values regardless of the
  caller's `sensitive` flag (`project_settings::looks_like_secret`) — extend the
  prefix list, never trust the UI checkbox. Mesh trust is an allowlist enforced at
  two gates (transport allowlist and the gossip signer check).
- PQ transport: the mesh QUIC transport offers hybrid X25519MLKEM768 first via an
  explicit `.crypto_provider(...)` — mandatory, because iroh's `N0`/`Minimal`
  presets prefer plain `ring` (zero PQ) whenever both TLS backends are compiled in.
  Report the REAL negotiated group (`hive_p2p::pq_kex_stats`). **Transport
  IDENTITY stays classical** (`EndpointId` is ed25519) — never claim post-quantum
  or ML-DSA identity. Public HTTPS/DB-gateway/relay TLS stays `ring`.
- TLS resumption is fleet-shared (`FleetTicketer` in `acme.rs`, key derived from
  `HIVE_SECRET_KEY`, 6h rotation, current-or-previous epoch, 0-RTT off, disabled
  with `HIVE_TLS_SHARED_TICKETS=0`) because rustls defaults to an in-process ticket
  store. Keep the full 4-cert chain (both ISRG roots are the cross-signing bridge);
  do not chase OCSP stapling — these leaves carry no OCSP URI.
- Mesh resource bounds: `HIVE_P2P_MAX_STREAMS` (256) is set explicitly to agree
  with `max_concurrency`, and the connection semaphore permit is acquired BEFORE
  the spawn so it bounds live connections rather than accept rate. iroh-relay
  1.0.2's `accept_conn_limit`/`accept_conn_burst` are unimplemented — setting them
  would look like a cap while enforcing nothing.
- Reserved proof names (`_acme-challenge`, `_hive-verify`, any case) are rejected
  in add/import/zone-parse so the proof channel is unforgeable; wildcards follow
  RFC 4592 (never at the apex, never over reserved names).

## Hashing: content identity vs. security binding

**BLAKE3 is the platform content-addressing hash.** `fluid_core::content_hash`
/ `ContentHasher` mint every NEW content identity; `fluid_core::verify_content`
verifies a stored digest under EITHER BLAKE3 or the legacy hex(SHA-256) over
the same domain. Nothing here is a flag day.

**hex(BLAKE3) and hex(SHA-256) are both 64 lowercase hex characters**, so no
shape check anywhere in the tree can tell them apart — `validate_sha256`,
`validate_digest`, `is_hex64` all prove shape and never algorithm. That is the
whole hazard: a rehash is undetectable by inspection, so it must be detectable
by construction. `verify_content` is the ONLY thing allowed to decide which
algorithm a stored digest came from.

**A digest that NAMES stored bytes must not have its writer flipped** until
every reader dual-accepts: a runtime-artifact store directory, a content-
addressed filename, a GC keep-set entry. Flipping one makes every object
already written read as a miss or an orphan. A digest that is only a LOOKUP
KEY (the build cache key, `hive-build-cache-v4`) is the exception — a miss is
merely a miss, so it moved to BLAKE3.

### Exclusion list — SECURITY BINDINGS, never touch the algorithm

The hash is the security property here, not an identifier for content.

- **Signatures (ed25519).** `hive-cloud/src/integrity_signer.rs:19,24,47,60,80`
  (per-node keypair; `:80` signs the chain head) ·
  `hive-core/src/integrity.rs:32,161,168,180` — **this SHA-256 chain fold looks
  like content addressing and is not**: its output is the message
  `integrity_signer.rs:79` signs, so changing the fold invalidates every
  signature ever made. · `hive-p2p/src/lib.rs:319,328,379,729` (gossip
  signature domain + verify) · `hive-cloud/src/gossip.rs:2017` ·
  `hive-cloud/src/browser_admission.rs:785,1034,1826` ·
  `hive-browser/src/lib.rs:1174,1331,1340,1359,1372,1387,2503,2565` ·
  `hive-p2p/src/dht.rs:33` · `hive-cloud/src/discovery.rs:16` ·
  `hive-cloud/src/admin.rs:8692` (transport identity stays classical ed25519 —
  no ML-DSA enrollment exists in this tree).
- **MAC / HMAC.** `hive-cloud/src/admin.rs:8061,8067` (`hmac_sha256`, shared by
  GitHub + Stripe webhook verification) · `hive-cloud/src/main.rs:1681,4064`
  (mesh hot-join HMAC over the endpoint id) ·
  `hive-cloud/src/git.rs:10825,10851,10863,10879` (artifact authenticity
  signature) · `webhooks.rs:15,186` · `billing.rs:1480,1854` ·
  `db_rest.rs:1374` · `tencent_eip.rs:28,303-338` (TC3-HMAC-SHA256) ·
  `hive-p2p/src/lib.rs:329,3858`.
- **TLS / certificates.** `hive-cloud/src/acme.rs:284-290` (`FleetTicketer`,
  key derived from `HIVE_SECRET_KEY`) and `acme.rs:1262` (ACME key
  authorization / JWK thumbprint).
- **Credential hashing.** `apikeys.rs:10,13` ·
  `browser_db_rest.rs:138-141,209,256` (bearer-token hash, a DB lookup key) ·
  `drive_api.rs:485-489,529,588,646,673` (share tokens + WebDAV passwords) ·
  `world.rs:1096-1101,1054` (hook-token index) · `enterprise.rs:281-324`
  (Argon2id is current; the legacy `sha256(project ":" password)` verify branch
  must keep working for every value already stored) · `zkauth.rs:44,265` and
  `hive-zkauth/src/lib.rs:34,231,243,260` (SHA-512, zk proof).
- **Handshake / supply-chain proofs.** `hive-backend/src/firecracker.rs:3471`
  and `hive-cell-agent/src/main.rs:336` (`agent_handshake_transcript`) ·
  `hive-cloud/src/build_executor.rs:4290` (`runsc.sha256`, checked against a
  published value) and `:4308` (`image_digest` parses a literal `sha256:` OCI
  digest — OCI is SHA-256, full stop).

### Content identity — already BLAKE3, do not move

`fluid-core/src/lib.rs:837-838` (`browser_source_digest`) and `:851-905`
(`browser_policy_digest`, mirrored byte-for-byte in
`hive-browser/www/function-runtime.js`) ·
`hive-browser-proto/src/lib.rs:133` (`BLAKE3_DIGEST_LEN = 64`), `:316` ·
`hive-cloud/src/browser_artifacts.rs:449-462,485-503,954-963` (policy digest is
the store filename, the route, and the ETag) ·
`hive-cloud/src/browser_db.rs:1694,1719,1720-1732,1864,2003,2076` (fragment,
membership and plan digests) · `hive-cloud/src/guardian.rs:3396`
(`iroh_blobs::Hash` is BLAKE3 internally) ·
`hive-cloud/src/git.rs:10708-10763` (build cache key, `hive-build-cache-v4`).

### Content identity — still SHA-256, writer deliberately not flipped

Readers dual-accept where marked; the writer stays until every node can verify
both. `hive-backend/src/runtime_artifact.rs:142,239-243,302,315,1105,1621,2151`
— `content_sha256` IS the store directory name and binds
`semantic_tree_sha256`, so a rehash orphans every staged artifact ·
`hive-cloud/src/deployment_ledger.rs:762` writer / `:805` reader (dual-accept;
this file already survived fc-phoenix crash-looping 21,679 times on exactly
this class of change) · `fluid-core/src/lib.rs:3152`
(`normalized_manifest_sha256`, consumed by `git.rs:5746`,
`runtime_artifact_transfer_wire.rs:497,519`,
`runtime_artifact_transfer_sender.rs:107` — all dual-accept) ·
`fluid-build/src/deployment_snapshot.rs:305` and `repository_snapshot.rs:303`
(cross-node sealed snapshot digest) · `hive-cloud/src/repository_build.rs:196` ·
`app_discovery.rs:128` · `build_executor.rs:889` ·
`billing.rs:677-702` (ledger checkpoint chain).

Verification of this section ran from a scratch crate OUTSIDE the repo against
the real on-disk ledgers, not a test file — see the commit for the witness.

@.gm/next-step.md
