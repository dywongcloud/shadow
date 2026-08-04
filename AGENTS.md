# AGENTS.md

Working conventions for this repo, present-tense — what must/must-not be true
of the code now, not a history of how it got that way (see `CHANGELOG.md` for
history).

## Data model

- Relational mirror on GuardianDB: 11 tables (projects/billing + view-only
  teams/members/deployments via `spawn_relational_mirror_loop`); billing is
  normalized real per-row tables (`billing_accounts`, `billing_ledger`,
  `billing_invoices` + `billing_invoice_lines`, `billing_checkouts`), not
  JSON blobs — `billing_ledger_snapshot`/`billing_invoices_snapshot` remain
  in the DDL deprecated-pending-removal (no longer written); hot-path
  `MetricsStore` deliberately excluded; `refresh()`-before-read requirement.
  `relational::upsert_billing` wraps a tenant's whole write (account + every
  ledger/invoice/invoice-line/checkout row) in one `BEGIN; ...; COMMIT;`
  transaction — never split back into separate autocommit statements.
  Detail: `recall("relational-scope")` / `crates/hive-cloud/src/relational.rs`.
- Admin SQL/tables view (`GET/POST /v1/admin/sql/*`) must stay read-only;
  extend `relational::known_tables()` for any new relational table. Detail:
  `recall("sql-readonly-guard")`.

## Mesh networking & anti-entropy

- Per-node embedded relay + `select_relay_hint` selection order; discovery
  fallback (`dial_fresh`) is complementary, not redundant, with the
  anti-entropy loop. New cross-node RPC paths go through `gossip::dispatch`'s
  existing match-arm pattern. Detail: `recall("relay-antientropy")` /
  `crates/hive-cloud/src/main.rs` (`spawn_anti_entropy_loop`), `crates/hive-edge`.
- **`PeerPool` keys trunks by ENDPOINT ID, never by the caller's label.**
  `acquire` parses the canonical id out of `addr_json` (which it must parse to
  dial anyway) and keys on that, with an alias map so name-only callers still
  resolve. This is load-bearing: the DATA plane passes node NAMES
  (`NodeInfo.id` = `args.name`) while the CONTROL plane passes 64-hex
  `peer_id`s, so a label-keyed pool held TWO QUIC connections to every peer —
  double handshakes, `relay_stats()` double-counting, the trunk warmer warming
  only the name-keyed half (leaving the control-plane trunk permanently cold),
  and `close_peer(eid)` unable to evict the edge's name-keyed trunk.
- **UDP over the mesh is RELIABLE and ORDERED, not datagram semantics — a
  deliberate tradeoff, not an oversight.** `read_raw_datagram` /
  `write_raw_datagram` carry each UDP payload as a `[u32 len][bytes]` frame on
  ONE QUIC bi stream (`RAW_MAX_DATAGRAM` = 65507). Boundaries and the size cap
  are preserved correctly, and the cancellation handling is deliberate:
  `read_raw_datagram` is NOT cancel-safe, so the inbound side gets its own task
  rather than a `select!` arm. The cost is real and worth knowing before
  putting latency-sensitive traffic on it: a lost packet now head-of-line-blocks
  every subsequent packet instead of being dropped, trading packet loss for
  unbounded latency — the opposite of what game or DNS traffic wants. iroh's
  own guidance for this case is stream-per-item aborted with `reset`/`stop`,
  and `Connection::send_datagram` does exist. Switching would mean giving up
  the single-stream framing (and its ordering guarantee, which the current
  consumers may rely on), so it needs a measurement first, not a rewrite.

## Round-robin reads vs leader-forwarded writes

- `admin_ingress` forwards MUTATIONS (POST/PUT/DELETE/PATCH) to the current
  control-plane leader but serves every GET/HEAD **locally**, and the public
  `api.<domain>` / dashboard hosts are round-robin DNS across all nodes. So any
  endpoint whose data is NODE-LOCAL — a file under `persist::data_dir()`, an
  in-process `Mutex`/`RwLock`/`OnceLock`/broadcast channel — that is WRITTEN by
  a mutation and READ by a GET will read the wrong node and return
  empty/404/0, silently and permanently. This one split has caused the zkauth
  preview lockout, blob-storage 404s, `queue_depth` always 0, and pub/sub
  delivering to nobody.
- **Every new endpoint must declare which side it is on.** A GET backed by
  node-local state needs an owner/leader proxy fallback — model it on
  `build_get` / `fetch_from_host` / `fetch_bytes_from_host` (binary payloads) /
  `sandboxes_api::proxy_to_owner` — or the state must move into a replicated
  store (`store_sync::REGISTRY`) or fan out (`db_replicate::fanout_all`, plus a
  matching `apply_mirrored_write` arm). A cross-node read added via
  `fetch_from_host` also needs its `gossip::dispatch` arm, ordered
  longest-prefix-first so a broader arm doesn't shadow it.
- Verify the fix by writing through the public round-robin host and reading it
  back several times, AND directly against a node still running the previous
  binary — the old node must still fail. That contrast is the proof.

## Secrets

- `HIVE_SECRET_KEY` is the fleet-shared at-rest key. **Never introduce or
  rotate it without carrying the previous key in `HIVE_SECRET_KEY_OLD`**
  (comma-separated hex): `secrets::load_or_create_key` prefers the env var over
  the persisted `$HIVE_DATA/secret.key`, so adding it orphaned every
  previously-sealed value fleet-wide — and `decrypt` returns its input
  unchanged on AEAD failure, so callers silently received raw `enc:v1:`
  ciphertext instead of the secret. `secrets::audit_at_rest()` logs any value
  no configured key can open at boot; `try_decrypt` is the honest,
  `Option`-returning variant for anything that must distinguish plaintext from
  undecryptable ciphertext.

- `ProjectStore::put_env` force-masks credential-shaped values regardless of
  the caller's `sensitive` flag (`project_settings::looks_like_secret`) —
  extend the prefix list for new providers rather than trusting the UI
  checkbox alone. Detail: `recall("secret-detection")`.

## Containers: the podman lock pool

- podman allocates one lock from a **fixed pool** (`num_locks`, default 2048)
  per CONTAINER **and per VOLUME**. The pool is per-host and shared by every
  tenant, so leaking locks starves the whole node: witnessed 2032 leaked
  volumes + 16 containers = exactly 2048, `freeLocks: 0`, after which NO
  container could start on that node and every cold start surfaced to users as
  503 `CAPACITY_EXHAUSTED` — a message blaming capacity for a leaked host
  resource. Because volumes count too, reaping containers alone does not fix it.
- **Any code path that removes a container must pass `-v`**
  (`container_cli::rm_args`), or it reclaims the container's lock and leaks its
  anonymous volume's lock forever.
- **Never `podman volume prune`** to reclaim locks. It deletes unused NAMED
  volumes, which is customer data — `hive-vol-postgres` and
  `hive-vol-minecraft-server-3` were both sitting prunable when this was
  diagnosed. Reclaim is gated on `is_anonymous_volume` (exactly 64 ascii-hex,
  podman's anonymous-id shape) AND `dangling=true`.
- Raising `num_locks` in `containers.conf` does **nothing** on its own: podman
  sizes the pool once and only re-sizes on `podman system renumber`. A config
  claiming a bigger pool while `podman info` still reports the old `freeLocks`
  is inert, not effective.
- `spawn_container_lock_sweep` (every node, not leader-only — the resource is
  per-host) reclaims under `LOCK_HEADROOM_FLOOR` and WARNs with the counts; the
  container-run path also self-heals reactively on `is_lock_exhaustion`.

## Crash-looping deployments

- A deployment can fail in two shapes that need **separate** handling, and
  conflating them is how one misconfigured app churned a host unchecked:
  the instance **starts then dies later** (`cold_start` returns `Ok`, tracked by
  `crash_streak` + `last_warm_ok_ms`), or it **never listens at all**
  (`cold_start` returns `Err` — "exited before listening on 127.0.0.1:PORT",
  tracked by `warm_fail_streak`). Never clear a failure streak merely because a
  start succeeded; only surviving `CRASH_LOOP_WINDOW_MS` counts as healthy.
- Both the autoscaler AND the request path must record cold-start failures. A
  pool with `min_instances=0` is never warmed, so keep-warm alone counts
  nothing and every request re-runs the doomed cold start.
- A broken deployment must report `DEPLOYMENT_CIRCUIT_OPEN`, never
  `CAPACITY_EXHAUSTED` — `classify_lease_error`'s `else` arm is a catch-all that
  otherwise blames the host for an app-level fault. Circuits are half-open by
  construction (backoff expires → next request probes → success clears), so a
  fixed deployment recovers with no operator action.
- **A circuit's open window must outlast the failure it guards.** Reusing the
  keep-warm backoff (2–8s) to gate a circuit against a ~20s failure meant every
  arriving request found the window already expired and paid the full cost
  again; `CIRCUIT_PROBE_INTERVAL_MS` is separate from `warm_backoff_until_ms`
  for exactly this reason.

## Request cancellation is a failure mode, not an edge case

- **Anything a request path reserves must be released by a `Drop` guard, never
  only on the `Err` branch.** When a client gives up (curl timeout, browser
  navigation, upstream proxy deadline) axum DROPS the request future mid-await.
  A dropped future never returns `Err`, so no error branch anywhere runs — every
  "release on failure" that lives in a caller is silently skipped.
- This wedged a whole function pool: the `provisioning` reservation taken in
  `decide_lease` leaked on every abandoned cold start, and because
  `live_count()` counts `provisioning`, the pool both refused to cold-start
  (every later request coalesced to the lease deadline instead of trying) AND
  looked already-at-`min_instances` to keep-warm, so it stopped being warmed.
  Nothing running, nothing counted, no recovery. `ColdStartGuard` is the fix and
  the pattern to copy.
- Corollary for counters that drive policy: count at the ONE chokepoint every
  caller funnels through, not in each caller. Callers that coalesce or wait
  (`LeaseDecision::WaitForWarm`) never reach their own error branch, and under
  load they are the majority — a streak counted per-caller stays stuck near
  zero and any threshold built on it never fires.

## Tenant tier (plan) is stored twice — write it through one helper

- A tenant's tier lives in BOTH `c.teams` (feature gating via `team_plan()`) and
  `c.billing` (project/seat quotas, the `can_deploy` credit lock, and everything
  the billing UI shows). Neither is authoritative on its own, and which half you
  read decides what the platform believes.
- **Every tier change goes through `admin::apply_plan_everywhere`.** Four call
  sites used to write only the billing half — the free-plan checkout shortcut,
  the Stripe `customer.subscription.deleted` downgrade, the operator grant, and
  checkout confirmation — so the halves drifted apart silently in BOTH
  directions: a completed upgrade left `teams` behind, and a downgrade left it
  high. Witnessed live: a tenant reading `enterprise` for features while being
  quota-limited and seat-capped as `hobby`.
- `teams.set_plan` returning `None` is normal, not a failure: personal
  namespaces (`personal`, `u_<uid>`) have a billing account and no team row.
- Same both-halves-or-neither rule on deletion — `team_delete` clears the
  billing account with the team record. The billing LEDGER is deliberately kept:
  it is the financial record and must outlive the account it describes.

## PVM kernels (KVM without hardware virt)

Cloud VMs with no `vmx`/`svm` get `/dev/kvm` from the out-of-tree PVM kernel
(`dywongcloud/pvm-no-fsgsbase-rdtscp`).

- **`pti=off` is REQUIRED.** With PTI active `kvm_pvm` refuses to load
  ("Support for host KPTI is not included yet") and `/dev/kvm` silently
  disappears, taking Firecracker with it. Fleet convention (see fc-virginia).
- **A minimal defconfig cannot host containers or microVMs** — the full
  required symbol set (netfilter legacy splits, USER_NS/MEMCG, cgroup2 BPF,
  vsock, FRED, vendor-KVM-off), which patch applies onto which tree, and the
  `make install` grubby-reset gotcha: `recall("PVM kernel build config")`.
- Verify functionally, not by device-node existence: open `/dev/kvm` and do a
  real `KVM_CREATE_VM` + `KVM_CREATE_VCPU`, then actually run a container.
- **`KVM_CREATE_VM` succeeding does NOT mean microVMs work — booting one can
  hard-reset the host, taking every tenant on it.** The ioctl check is
  necessary and still not sufficient; the microVM boot is a SEPARATE gate.
  Witnessed for real on fc-frankfurt (2026-07-31, kernel 7.1.5): 3 host resets,
  root-caused to a stack-corruption bug in the fork's switcher assembly —
  full analysis in `docs/pvm-upstream-report.md` (§8), status in
  `docs/blocked-work.md`. That node now runs `HIVE_FORCE_MOCK=1` so the
  backend auto-select cannot pick Firecracker there.
- Consequently `pvm_run_smoke_test` is dangerous on an unproven host and must
  never run against a node already carrying traffic — bring-up only, before
  the node joins the mesh. Its position at the END of `pvm_firecracker` (a
  crash there leaves the node with no hive-cloud at all) and the
  `--skip-tags smoke_test` workaround: `recall("PVM smoke-test hazards")`.

## Bringing a node into the mesh

- **Seed `HIVE_BOOTSTRAP_PEERS` with ADDRESSED peers, never bare node ids.** The
  format (`hive_p2p::parse_seed_addr`) is
  `<64hex-id>[@ip:port[+ip:port]][|relay-url]`; a bare id is accepted but then
  cold-start rendezvous depends entirely on n0/Seer discovery resolving it,
  which is not reliable. Witnessed: three nodes with bare-id seeds served
  healthz 200 and opened 25 mesh trunks, yet each saw ONLY ITSELF in
  `/v1/nodes` while the rest of the fleet couldn't see them either — invisible
  to the dashboard, to placement, and to DNS. Relay-addressed seeds
  (`<id>|https://<node>.relay.shadw.app:3343`) converged the registry
  immediately. The relay URL form matters: with TLS enabled the standalone
  iroh-relay serves EVERY relay service (`/relay`, `/ping`, QAD) on the https
  socket only — the plaintext `:3340` listener is captive-portal-only, so an
  `http://<ip>:3340` relay URL is a dead hint (and an IP literal can never
  pass the `*.relay.shadw.app` cert check; use the DNS name).
- `peer_iroh.json` stores each peer's **private** addrs (10.x/172.16/192.168),
  so copying a populated peer book from one node to seed another does not give
  a dialable address — use public IPs / relay URLs for seeds.
- A node's own trust drop-in must list EVERY fleet id (`HIVE_TRUSTED_NODE_IDS`
  + `HIVE_PEER_TRUST=1`), and so must every existing node's — gossip is
  non-transitive and the trust list is an allowlist.
- Health probes run over **iroh**, not HTTP, so a restrictive cloud security
  group that blocks 8786/8787 does NOT by itself make a node unhealthy — but it
  does stop HTTP admin dispatch, which pushes deploys onto the iroh path.
- **A health verdict is per-OBSERVER, so peers legitimately disagree and a
  probe-side fix only helps the node running it.** `spawn_health_loop` probes
  from the node it runs on and writes that node's own registry, so "is X
  healthy" has no single fleet answer — a rollout improving probe behaviour
  changes the upgraded node's view of everyone else, and nothing about how
  anyone else sees IT. Two operational consequences. (1) Diagnose from several
  vantages before believing any one node: measured live 2026-07-31, one peer
  reported fc-virginia-2 unhealthy while three others reported it healthy, and
  the single dissenting reading was the wrong one. (2) The verdict that
  actually decides traffic is the **control-plane leader's**, because DNS and
  placement are driven from there — a node every other peer can see is still
  effectively dark if the leader alone cannot probe it.
- Cross-continent probes are genuinely slow, and a healthy one can far exceed
  the default 2s `HIVE_HEALTH_TIMEOUT`: a real, SUCCESSFUL sj probe measured
  7462ms. Treat a low timeout as a correctness knob, not just a latency one —
  set too tight it manufactures unhealthy peers out of working links.
- **Never trust a TCP port probe run from a laptop on the VPN.** It
  SYN-proxies, so a closed port reads OPEN. Witnessed: a laptop reported
  hk:3340 open while every fleet vantage correctly reported it unreachable.
  Probe from a fleet node, always.

## Serverless GPU

- GPU serving is the **container path only**: Firecracker has no PCI
  passthrough, but on FC nodes containers run via host podman, which is where
  the GPUs live. A gpu launch adds CDI `--device nvidia.com/gpu=all` (from the
  nvidia-container-toolkit spec at `/etc/cdi/nvidia.yaml` on GPU hosts); the
  existing retry-on-default-runtime fallback covers runsc-without-nvproxy.
- The request flows `FunctionSettings.gpu` (project toggle) OR
  `fluid.json functions[].gpu` → `FunctionConfig.gpu` → placement →
  `FunctionLaunch.gpu` → podman. Settings can turn GPU **on** but never strip a
  function's own declared need (the manifest build ORs them).
- Placement: gpu deployments are eligible **only** on nodes advertising
  `NodeInfo::gpu_count > 0` (boot `nvidia-smi` probe, `HIVE_GPUS` override) —
  including the lease-stickiness path. Deliberately NO silent fallback to CPU
  nodes; empty placement beats cold-starting into CUDA errors.
- GPU time is metered as instance **wall-time** (`fluid_ms`), not active-CPU —
  the GPU is held for the instance's whole life. `RateCard.gpu_hr_cents`.
- Fleet-roll gotcha (cost a silent stale build): `rsync -a` preserves local
  mtimes, so a build that finishes **after** a sync lands can leave cargo
  fingerprints newer than the fresh sources — cargo then silently skips
  rebuilding the changed crate. If a just-rolled node still runs old behavior,
  `touch` the synced sources before rebuilding.

## Geo-DNS (Seer)

- **The delegation boundary is the whole story.** `shadw.app`/`shadw.cloud` are
  delegated to `ns1/ns2.vercel-dns.com`, and Vercel DNS is plain authoritative
  DNS with **no geo or health routing** — so the platform's own geo-aware
  server (`dnsserver.rs`, "Seer") can only answer for names actually delegated
  to it. `vercel_dns::desired_geo_delegation` publishes that delegation (NS on
  the deploy-zone label + `ns-<node>` glue) into the Vercel-hosted parent from
  the same health-damped `PublishNode` set every other record uses, so a
  nameserver that goes unhealthy leaves the NS set by the normal diff. It
  refuses to publish below **two** nameservers — a one-NS zone is a single
  point of failure for every name under it.
- **Advertise only what peers have PROVEN, never what a node claims.**
  `NodeInfo::dns_ns` is necessary and never sufficient — it is read from the
  node's own env and says nothing about reachability. Trusting it alone put two
  DEAD nameservers into the live delegation. `dns_probe` (every node, not
  leader-only) proves reachability from independent vantages and gossips
  passers as `NodeInfo::dns_attest`; `validate_nameservers` admits a node only
  while attested from two distinct REGIONS, never self-attesting. Below two
  PROVEN nameservers the reconciler **HOLDS rather than withdraws** — deleting
  every NS would turn a degraded delegation into a blackholed zone — and the
  hold opens an incident. Expect a rollout to HOLD until two regions' worth of
  provers are up: that is the designed direction of failure, not a regression.
  Full mechanics (the exact probe bar, the rotating client-subnet sample and
  why "responds" isn't it, the damping K, the `--dns-probe` diagnostic):
  `recall("Geo-DNS Seer nameserver attestation")`.
- **Two tailoring inputs, one rule.** `dns_geo.rs` locates the client by EDNS
  Client Subnet when the resolver sends one, else by the query's source
  address. `GeoCache` **never blocks the DNS loop**, and the primary geo source
  is LOCAL: `geoip.rs` binary-searches a committed prefix→coordinate table
  (`crates/hive-cloud/assets/geoloc.bin`, `include_bytes!`, ~178 ns/lookup,
  5.3 MB of demand-paged `.rodata`, zero heap), so the FIRST query for a prefix
  is already tailored and no third party sees a client prefix. A tailored answer
  must carry a non-zero ECS scope (= the responding prefix length) so recursives
  cannot reuse it for clients it wasn't computed for; a generic answer echoes
  scope 0.
- **The remote geo endpoint is OFF by default and must stay optional.**
  `HIVE_DNS_GEO_ENDPOINT` is now the only way any geolocation HTTP call happens
  — there is no default third-party service left on the DNS data path. Set, it
  covers only prefixes the local table misses, still on the paced background
  worker (generic answer now, memoised answer later). Never re-introduce a
  default endpoint: the table is what makes a node with no egress route
  correctly. Refresh the table with `scripts/gen-geo-table.py` (DB-IP City Lite,
  CC-BY-4.0) and commit the blob; it is an input, not a build artifact.
- **The GeoCache remote memo is durable, and only the background half touches
  disk.** Entries persist to `$HIVE_DATA/dns_geo.json` (its own sidecar, NOT
  part of `PlatformSnapshot` — node-local derived data must not ride the
  platform-state write path or replicate across the mesh) so a restart does not
  re-generic every remotely-located prefix; a previously-known prefix is
  tailored on the FIRST query after boot. Writes happen on a debounced
  background tick (`HIVE_DNS_GEO_SAVE_MS`, default 10s, `0` disables
  persistence entirely) plus a `flush_blocking()` on SIGTERM next to
  `persist::flush_blocking` — never on the DNS hot path. Entries age out
  (Known 30d, Unlocatable 6h, Pending 60s), an expired Known is SERVED while
  its refresh is queued (expiry must not cause the de-tailoring blip
  persistence exists to remove), a failed refresh never downgrades a location
  already held, and `MAX_ENTRIES` (8192) is enforced on LOAD as well as at
  runtime so no on-disk file can reload past the cap.
- **Health beats proximity, always.** `lb_records` filters to healthy nodes
  with a public address of the requested family BEFORE proximity ordering, so
  the nearest-but-dead node is never returned.
- **Apps zone: affinity first, then proximity.** When `HIVE_DNS_SERVE_APPS` is
  on, Seer answers the customer zone with the same two-tier rule the published
  records encode — a host attributable to a specific node resolves to THAT node
  (the deployment runs there; anywhere else buys a cross-node forward), and
  everything else gets the proximity-ordered healthy set. Off by default: it is
  only meaningful once the zone is delegated here.
- **Env that matters:** `HIVE_DNS_ADDR` (bind; `0.0.0.0:53` in prod),
  `HIVE_DEPLOY_ZONE` (the delegated geo zone), `HIVE_DNS_SERVE_APPS`,
  `HIVE_DNS_GEO_TABLE` (path to a replacement geo table; falls back to the
  embedded one if missing/corrupt), `HIVE_DNS_GEO_ENDPOINT` (optional remote
  lookup for table misses; unset = no third-party call at all),
  `HIVE_DNS_GEO_SAVE_MS` (geo cache debounce; `0` = no persistence),
  `HIVE_DNS_PROBE_SECS` / `HIVE_DNS_PROBE_TIMEOUT_MS` (nameserver prover
  cadence + per-query budget). **Gotcha that cost real
  debugging time:** a systemd DROP-IN (`hive-node.service.d/seer.conf`) can
  override the main unit's value — the unit file read correct while the RUNNING
  process had a typo'd zone, silently keying the entire geo path on a domain
  nobody owns. Verify with `/proc/<pid>/environ`, never the unit file.
- **Failure modes:** Seer down → delegated names go dark (hence the ≥2-NS
  rule); geo table corrupt or a prefix it cannot place → generic answers, never
  an outage; remote endpoint (if configured) down → generic answer plus a
  queued lookup that fails harmlessly;
  corrupt/unreadable/oversized/wrong-version geo cache file → empty cache and a
  WARN, never a boot failure. `GET /v1/dns/stats` (operator) exposes query
  counts, the tailored-vs-generic split, the loaded table's source + row counts
  + local hit/miss, the remote memo's known/pending/unlocatable plus
  `cache_loaded_at_boot`/`cache_writes` (the answer to "did the cache survive
  the restart"), published delegation-record count, which node each answer
  handed out first, the per-node nameserver VERDICT (declared / validated /
  attesters / attester regions / reason — the same `validate_nameservers` call
  the reconciler publishes from, so the two can never disagree), and this
  node's own raw probe evidence.

## DNS cutovers & ACME (reconciler invariants)

- **A delegated name must never serve NEITHER addresses NOR delegation.**
  Vercel forbids NS records coexisting with any other record on a name and
  refuses NS creation while any child record exists (409
  `record_conflicts`) — an orphaned `_acme-challenge.api` TXT vetoed the
  entire `api.shadw.cloud` cutover and stranded it dark for 90 minutes
  (2026-07-29). The rule is one-directional: child-record CREATES under an
  existing delegation are permitted (inert/occluded), and an NS RRset
  always grows by adding members, so a foreign NS target never blocks.
  Every cutover/disengagement runs as a restore-on-failure transaction in
  `vercel_dns::plan_writes`: the address set plus any squatter
  (CNAME/ALIAS/stray TXT) is removed, the NS set created, and on any NS
  failure everything is restored. Disengagement hoists NS deletes only for
  names with no desired NS and symmetrically restores the delegation when
  the flat-address creates all fail; a rotation on a still-delegated name
  keeps creates-then-deletes order (replacement before removal).
- **The ACME orphan sweeper runs every reconcile pass.** Issuance cleanup
  races Vercel's eventually-consistent listing, so a finished order's TXT
  can survive and then veto future delegations from under its parent name.
  Any `_acme-challenge.*` TXT unknown to the in-flight challenge store and
  provably older than 15 minutes is deleted; `created` is schema-nullable,
  and unknown age means KEEP (deletes are forever). acme.rs's Vercel-side
  TXT create is best-effort ONLY on a 409 under a LIVE delegation gauge
  (`STATS.geo_delegation_records` / `STATS.api_delegation_records`), never
  static zone config — below the capable-NS floor the flat set is
  authoritative again and a swallowed failure fails orders opaquely.
- **publishable() is damped in BOTH directions.** Withdrawal stays fast
  (K=2 consecutive unhealthy passes); re-addition requires
  `HEALTHY_PASSES_BEFORE_REPUBLISH` consecutive healthy passes — a
  flapping node otherwise drives a create/delete treadmill against the
  Vercel API (429s plus transient address-record loss) during every fleet
  reconvergence.
- **Let's Encrypt rate limits are incidents, not warn lines.** The
  duplicate-certificate window (5 per 168h per exact identifier set)
  closes a bundle's renewal — including the `acme-force-*` sentinel —
  until it opens; a `rateLimited` issuance error opens a Major incident
  naming the bundle and the window.

## Managed inference (serverless GPU pooling)

- A project opts in with a `fluid.json` TOP-LEVEL block:
  `{"inference": {"model": "<direct GGUF URL or org/repo/file.gguf HF path>",
  "pool": true}}`. The deploy path syncs it into
  `ProjectSettings.inference`; `inference::spawn_reconcile` (every node) does
  the rest — the app itself needs NO GPU code and NO GPU placement: it just
  reads `HIVE_INFERENCE_URL` (leader-injected env, same precedent as DB env
  auto-injection) and speaks OpenAI protocol
  (`$HIVE_INFERENCE_URL/chat/completions`) from any framework.
- The backend is real llama.cpp: every GPU node runs `llama-rpc.service`
  (CUDA `rpc-server`, port 50052, peers-only). Per project, a COORDINATOR GPU
  node is elected deterministically (FNV(project) over the sorted GPU roster
  of the largest-VRAM region — every node computes the same answer from
  gossiped state, no election protocol) and runs `llama-server` on a
  deterministic port (`50100 + FNV(project) % 900`, range locked down
  fleet-wide). Models cache under `/root/models`.
- Placement is pool-aware, single-node-first: fits the coordinator's free
  VRAM (`gpu_pool` live figures) → runs alone; doesn't fit AND `pool: true` →
  `--rpc member:50052,...` layer-distribution across enough same-region
  members to cover; can't fit even the whole pool (or pooling disabled) →
  the endpoint parks `failed: <honest reason>` — never a silent CPU fallback,
  matching the GPU-placement rule above. `GET /v1/inference` (operator) lists
  every endpoint's coordinator/port/URL + the serving node's live statuses.
- Rebuild constraint from the node half: fc-sanjose-gpu-2 runs driver
  570.211.01 — any llama.cpp rebuild must stay on CUDA 12.x, not 13.x.

## Browser-function artifacts (build contract)

- **A function is browser-eligible ONLY by opting in** via fluid.json
  `functions[].browser` (`fluid_core::BrowserPolicy`: `entry` + bounded
  `mode`/`timeout_ms`/`memory_bytes`/`stack_bytes`/`allowed_ops`) and
  surviving `browser_artifacts::bundle` at build time. An opted-in function
  that is ineligible — container/python/go/command runtime, TypeScript or
  missing entry, any Node/Bun/Deno surface in the source (`require`,
  `import`/`export`, `process.`, `Buffer`, `Bun.`, `fetch(` in quickjs mode),
  an unknown host-op id — FAILS THE BUILD loudly; the deployment is never
  registered. Never "warn and drop the opt-in": that leaves the fleet serving
  code donors believe they serve.
- **The canonical policy digest is ONE contract with TWO implementations.**
  `fluid_core::browser_policy_digest` must stay byte-for-byte identical to
  `policyDigest` in `crates/hive-browser/www/function-runtime.js`
  (domain `hive-browser-policy-v1\0`, LE fields, sorted deduped op ids with
  their ABI strings from `fluid_core::browser_host_op_abi`). The policy digest
  binds the source digest to the exact limits and is THE wire digest
  (`encode_invoke`, `pin`'s return, the admission's `digest`) — a drift
  between the two implementations silently breaks every artifact pin.
- **Bytes stay node-local; only descriptor metadata replicates.** Artifacts
  live in `$HIVE_DATA/browser-artifacts/<policy_digest>.js` + a `.json`
  owner sidecar (deployment ids); the manifest carries only
  `FunctionConfig::browser_artifact` (digests, size, limits), which
  `DeploymentInfo.browser_functions` gossips. Artifacts never ride
  `PlatformSnapshot`/store_sync (the `dns_geo.json` precedent) and are
  deliberately NOT packed into the deliver_build ext4 — they execute in
  donors' browsers, carry no env/secrets.
- **Admission capabilities are entirely server-derived.** `browser_admission`'s
  `validate_request` resolves deployment+function to its ready descriptor
  under the authenticated tenant (`browser_artifacts::descriptor_for`) and the
  admission record's `digest` IS that descriptor's canonical policy digest;
  `AdmissionRequest.digest` is a rollout-compat field that is accepted but
  never read — a forged digest admits nothing (there is nothing to match it
  against), and a stale one is simply reconciled to the current descriptor.
  The admit/renewal response carries a `capability` block — `artifact_url`,
  `policy_digest`/`source_digest`/`source_bytes`, resolved limits, and
  `trusted_callers` — one atomic snapshot the donor reconciles its grants
  from, descriptor rotation included (`routing_identity_changed` tears the
  old route down first). The block is additive JSON on the admission HTTP
  API; `hive-browser-proto`'s QUIC wire contract is untouched.
- **A fresh validated admission supersedes the relay denylist; a bare revoke
  still denies.** `stop()`'s admission DELETE writes a relay-denylist entry
  (10-min retention, `RELAY_DENYLIST_RETENTION_MS`) so a revoked identity
  cannot sit on the embedded relay — and `BrowserAdmissionStore::put` now
  REMOVES that entry for the same endpoint id, because an admission that
  already passed fresh-session + PoP + descriptor validation is by
  definition the identity owner re-admitting, not a replay. Followers learn
  it within one mesh round trip (`fanout_deny_clear` → the
  `/v1/browser/admissions/mesh-deny-clear/` gossip arm, the
  `fanout_revoke` mirror; denylist-only on receipt, never versioned state),
  else on the next snapshot adoption. Revocation semantics are unchanged:
  without a new admission the entry stands for its full retention window
  (witnessed live: stop → same-identity reboot denied `browser admission
  revoked` → fresh admit → immediate reconnect). Caveat for browsers booting
  THROUGH an enforcing (embedded) relay: `BrowserNode.boot` gates on
  relay-online before the worker can admit, so a denylisted boot still fails
  there until the browser side admits pre-online — production browsers use
  the standalone relays, which run no AccessControl, so the deadlock is
  dev-only.
- **`trusted_callers` comes from the live registry, never client input.**
  Every HEALTHY node's proven iroh identity (parsed from the gossiped
  `EndpointAddr`, else the join-verified `peer_id`) — never node names, never
  browser ids, never a wildcard/TrustSet. Health-filtered by design: a fleet
  removal drops the id on the next renewal and a re-addition restores it, on
  the same snapshot the descriptor rotation rides.
- **Artifact delivery is a tenant-authorized content-addressed GET with an
  owner proxy** (`GET /v1/browser/artifacts/<policy_digest>`,
  `browser_artifacts::routes`). Serve only when the authenticated session's
  tenant owns a READY deployment referencing that exact descriptor
  (`resolve_for_tenant`; foreign tenant and unknown digest both 404 — no
  existence leak), and only after re-verifying exact size + source BLAKE3
  against the descriptor. The bytes live only on the build node, so a local
  miss proxies to a deployment host / the leader
  (`fetch_artifact_from_host`: HTTP admin with a `?local=true` no-re-proxy
  guard, else the gossip `/v1/browser/artifacts/` arm, which re-runs the
  tenant gate against the OWNER's own deployment state — the
  `proxy_to_owner` re-check precedent). Proxied bytes pass the SAME
  verification before serving — this node never emits unverified bytes under
  immutable headers (`Cache-Control: public, max-age=31536000, immutable`,
  ETag + `x-hive-policy/source-digest`). The digest is validated (64
  lowercase hex) before it ever becomes a path component — tenant path
  fragments get 400, never a filesystem lookup.
- **GC is guarded exactly like `gc_rootfs_images`.** The keep-set is the
  policy digests of ALL live local deployment records; `browser_artifacts::gc`
  refuses an empty keep-set and any reap set over
  `HIVE_BROWSER_ARTIFACT_GC_MAX_REAP_FRACTION` (default 0.5), and only reaps
  files older than `HIVE_BROWSER_ARTIFACT_GC_GRACE_SECS` (default 600). A
  legitimate full drain (every browser deployment deleted) therefore refuses
  forever by design — that is a state-versus-caller-bug ambiguity no GC may
  resolve by deleting; drain the store dir by hand.

## Browser-replicated databases (the `browser_db` contract)

The contract the browser↔fleet CRR exchange implements is
`docs/browser-db-contract.md`; the load-bearing invariants:

- **Opt-in is a fluid.json TOP-LEVEL `browser_db` block, one logical database
  per PROJECT.** `fluid_core::BrowserDbPolicy` (presence = opt-in, every field
  defaulted, ceilings clamp with notes via `resolve()` — never warn-and-drop)
  rides `Manifest::browser_db` → `DeployRecord` → and is stamped VERBATIM onto
  `DeploymentInfo::browser_db` for the `/v1/fleet-deployments` gossip view, so
  the admission-issuing leader and exchange peers resolve caps for deployments
  they do not host. The block replicates RAW and resolves at the point of use
  (the `InferenceSpec` precedent); a pre-upgrade peer carries no field and
  presents as not-opted-in — absent capability, never wrong capability.
  Database IDENTITY is the project (the `hive-vol-{project}` precedent: data
  survives redeploys); the spec and every grant are deployment-scoped.
- **Grants ride the admission, server-derived and tenant-pinned.** A `db`
  block on the admit/renewal capability, resolved from the deployment
  descriptor under the authenticated tenant, dying with the admission lease.
  Team scope = read+write; Public scope = read-only and only with
  `public_read: true` — an anonymous donor never writes tenant data. The
  exchange peer re-checks the grant against its OWN replicated
  `browser_admissions` view (the `proxy_to_owner` precedent).
- **Caps bind BOTH sides.** `max_bytes` (default 64 MiB, ceiling 1 GiB)
  per-replica on the browser OPFS copy AND each fleet file; `max_value_bytes`
  (default 1 MiB, ceiling 16 MiB) at the sync boundary. Over-cap = typed
  refusal + whole-batch rollback — NEVER truncate or evict rows to fit, which
  in an LWW store is silent permanent divergence.
- **The converged fleet replica set is the system of record; every browser
  OPFS copy is a cache of record** (replication factor zero). Revocation wipes
  the browser copy, expiry seals it until re-admission (watermarks resume),
  churn costs nothing. Fleet files live while the project lives: block removal
  stops grants and starts a 30-day inert grace, then GC with the
  `browser_artifacts::gc` guards (empty keep-set refuses, max reap fraction,
  grace age).
- **Naming is platform-templated, bytes never ride replicated state.** Fleet
  `$HIVE_DATA/browser-dbs/hive-browserdb-{sanitize_tag(project)}.db`, browser
  `/hive-crsql/<same>` — the volume-isolation invariant applied to DB files;
  no wire field ever names a file. Replica bytes/site-ids/watermarks replicate
  ONLY through the `hive_crsql` ChangeBatch protocol — never
  `PlatformSnapshot`/`store_sync`/a gossip snapshot arm (the `dns_geo.json`
  precedent), and no owner-proxy HTTP arm serves DB bytes: a file copy is not
  a merge.

The exchange itself (bn-browser-fleet-crr-exchange, landed):

- **One `Op::CrrSync` op on the existing `hive/browser/0` ALPN carries whole
  bidirectional rounds.** Request = the sender's per-site watermarks (the
  responder's export selector) + its outbound HCB1 batches; reply = a typed
  apply status (ok / sync-gap / quota-exceeded / value-too-large /
  read-only / batch-refused), the responder's watermarks AFTER apply (the
  acknowledgement the sender persists as its push cursor), and the
  responder's bounded export — `more` means re-request, and the requester's
  freshly applied watermarks ARE the resume cursor (no cursor state to
  lose). Sync-domain refusals are reply STATUSES; protocol faults stay
  stream reset codes. The op gets its own 4 MiB frame cap
  (`BROWSER_MAX_CRR_FRAME`, checked per-op before allocation on both
  halves); every other op keeps 1 MiB.
- **The fleet re-checks the grant on EVERY request against its own
  replicated admission view** (`browser_db::resolve_round_grant`): live
  admission + `db` grant on the record + the deployment descriptor STILL
  carrying the block, and for Public scope the LIVE spec still saying
  `public_read`. Unknown project, foreign tenant and block-removed are the
  identical FORBIDDEN — no existence leak. Tenant+project+file are derived
  server-side from the QUIC-authenticated endpoint's admission; the
  request's `db_file` field is a grant IDENTIFIER only (the browser echoes
  its capability's name; the fleet compares it against its own
  server-derived name and refuses a mismatch — a stale capability can never
  contaminate another project's replica, and the wire value never becomes a
  path).
- **cr-sqlite v0.17 does not replicate schema — both halves derive it from
  the spec.** `BrowserDbPolicy.schema` (`{name, ddl}` per table) rides the
  deployment record and is handed to the browser verbatim in the capability;
  the fleet reconcile applies the same DDL + `crsql_as_crr(name)` (which is
  idempotent) to the replica file. `val_payload_bytes` (hive-crsql and
  `hcb1.js`) is ONE formula with two implementations, same discipline as the
  HCB1 layout and the policy digest — the cap check must agree byte-for-byte
  on both sides.
- **The browser's push cursor is a non-CRR `hive_sync_meta` table INSIDE the
  OPFS file** (the fleet's acknowledged watermarks per site), so a tab
  reload resumes incrementally — witnessed: after reload, zero re-push and
  zero re-apply before the next write. Revocation cuts replication at the
  op-level re-check (already-open connections included), then the browser
  wipes its OPFS replica (the worker's `wipe` op → the VFS's own `xDelete`).
- **Fleet-fleet convergence flows through browser carriers in v1** (a Team
  browser that synced node A's changes pushes them to node B's replica —
  per-site watermarks make it the same protocol); a direct fleet↔fleet arm
  and server-pushed scheduling are deliberate follow-ups, not correctness
  holes.
- **Replica GC keeps the blast-radius guards** (`browser_artifacts::gc`
  verbatim): empty keep-set refuses the pass, reap set over
  `HIVE_BROWSER_DB_GC_MAX_REAP_FRACTION` (0.5) refuses, only files past BOTH
  `HIVE_BROWSER_DB_INERT_GRACE_SECS` (30 days, the block-removed/project-
  deleted retention) and `HIVE_BROWSER_DB_GC_GRACE_SECS` (600) reap.
- **Env that matters:** `HIVE_BROWSER_DB_LISTEN=0` disables the serve arm
  (rollout/ops: `Op::CrrSync` then gets NO_HANDLER — exactly a pre-change
  binary's refusal class); `HIVE_BROWSER_DB_RECONCILE_SECS` (30);
  `HIVE_CRSQL_EXTENSION_PATH` points at the packaged cr-sqlite extension on
  fleet nodes (the vendored build is the local-dev default; a missing
  extension is a loud WARN + refused rounds, never a boot failure).

## Tenant volume isolation (verified, keep it this way)

- **A project name can never become a host path, and that is load-bearing.**
  Podman reads `-v <a>:<b>` as a BIND MOUNT whenever `<a>` looks like a path, so
  any code that interpolates a tenant-controlled string into the left side of
  that pair is one bad name away from mounting host `/` into a hostile
  container. This platform is safe by construction rather than by luck:
  `container_volume_cfg` builds `hive-vol-{sanitize_tag(project)}`,
  `sanitize_tag` maps every character outside `[a-z0-9._-]` to `-` and trims
  leading/trailing `.`/`-`/`_`, and the `hive-vol-` prefix is prepended AFTER
  sanitization — so the left side is always a named volume, never a path, even
  for a project called `/`, `..`, or `../../etc`. `volpath` is a platform
  constant, not tenant config. Verified empirically 2026-07-31: 9 running
  containers across 5 nodes, **zero** host bind mounts.
- **Any future storage feature must preserve that invariant.** Snapshot,
  quota, restore and dataset-name paths all take tenant-supplied identifiers;
  each is the same footgun. Validate at the boundary and construct the final
  name from a platform-controlled template — never pass a tenant string
  through to a mount/dataset argument, and never accept one that has already
  been concatenated upstream.

## Storage capacity & placement

- **Placement must consider free disk, and disk is a HARD filter, not a score
  term.** `schedule.rs` filtered on health/region/GPU and then sorted by
  deployment COUNT — a metric that says nothing about space — so a full node and
  an empty one scored identically. Witnessed 2026-07-31: fc-sanjose reached 0
  bytes free and took 9 customer deployments down while fc-frankfurt and both
  CVM nodes sat under 10% used with ~920 GiB free each. `NodeInfo::disk_free_gb`
  is gossiped and refreshed on a timer (`HIVE_DISK_REFRESH_SECS`); a
  boot-time-only value would be worse than useless because it goes stale in
  exactly the direction that matters. Unlike CPU/memory, disk does not drain on
  its own once a deployment lands, so a weighted score would still let a full
  node win — hence the hard floor (`HIVE_PLACEMENT_DISK_FLOOR_GB`), set ABOVE
  the per-cold-start floor so admission does not simply defer the failure to the
  cold start, which then blames the customer's app.
- **`disk_free_gb == 0` / `gpu_free_mb == None` mean UNKNOWN, never "full".**
  A pre-upgrade peer reports neither. Treating unknown as exhausted empties the
  candidate set mid-rollout — the failure direction must always be "admit and
  let the cold-start floor catch it", never "silently exclude the fleet".
- **The per-deployment data images are named with a DOUBLE prefix, and it is a
  real bug that is now load-bearing.** `deliver_build` names them `dpl-{bid}`
  where `bid` is already `dpl-<hash>`, so every file is
  `dpl-dpl-<hash>.data.ext4`. A GC whose keep-set is built from deployment ids
  the obvious way matches NOTHING on disk: measured live, 0 of 369 stems matched
  raw deployment ids while 328 of 369 matched after stripping the extra prefix.
  Written naively, that GC deletes every live deployment's data disk.
  `gc_rootfs_images` therefore matches both forms AND refuses outright when the
  keep-set is empty or when more than `HIVE_GC_MAX_REAP_FRACTION` of images look
  orphaned. **Any future reclaim path needs the same guard** — a blast-radius
  check is the difference between a bug and an unrecoverable one.
- **The GC could not see what was filling the disk.** `gc_orphans` walked only
  `run_dir` (ephemeral per-cell dirs); the persistent `<image>.data.ext4` files
  were invisible to it, and nothing else deletes them (`terminate` removes only
  `cell.root`; the backend trait has no delete method at all). The headroom
  error said "after GC", which reads as "nothing left to reclaim" when in fact
  the GC had freed zero bytes while 325 GiB sat next to it — it now reports the
  actual reclaimed delta, so an INEFFECTIVE GC is distinguishable from an
  exhausted one.
- **Storage growth here is a retention question, not only a leak.** Of 369
  images on fc-sanjose only 41 (4.3 GB) were genuinely unreferenced; the other
  328 are still named by live platform state, which never forgets a deployment.
  Reclaiming beyond the orphan set is a policy decision (how many superseded
  deployments per project to keep), not something a GC should do silently.
- **Dedup/compression is NOT the lever here — measured, not assumed.** Sampling
  two same-apparent-size images found only **1.3%** of 1 MiB blocks shared:
  identical sizes, genuinely different content. ext4 has neither reflink nor
  `FIDEDUPERANGE`, so dedup would also mean an XFS/btrfs migration per node.
  Capacity-aware placement pays far more for far less risk. Re-measure before
  anyone revives this.

## GPU capacity accounting

- **Free VRAM must come from the driver, not from arithmetic.** The pool derived
  it as `total - (per_instance_reserve * live_gpu_instances)`, which drifted from
  the hardware in BOTH directions on the same fleet: fc-sanjose-gpu-1 was
  reported with 30 GiB reserved while `nvidia-smi` measured 59232 MiB actually
  free (phantom reservations), and fc-sanjose-gpu-2's resident `llama-server` was
  invisible entirely, because an inference endpoint is not a serverless function
  instance and is never counted. `NodeInfo::gpu_free_mb` carries the measured
  figure; the pool takes the MINIMUM of measured and estimated, so a reservation
  the driver has not yet materialised (instance mid cold-start) still counts as
  real pending demand.
- **Do NOT add `--split-mode tensor` on these GPUs.** llama.cpp already pools all
  local cards by default (`layer` split) — confirmed by measurement, VRAM spread
  evenly across all four T4s. T4 is PCIe Gen3 with no NVLink, where published
  numbers put 40-50% of inference time in transfer at TP=4, and llama.cpp's own
  Turing P2P/tensor path has a live crash history. Cross-node pooling via `--rpc`
  is correctly pipeline-parallel for the same reason: tensor parallelism needs an
  AllReduce per layer and is unusable over a WAN/QUIC link.

## Deploys

- `git push` auto-deploys through TWO independent triggers, never assume the
  webhook is the only one. (1) The GitHub webhook (`admin::git_webhook`,
  `/v1/git/webhook`) fires only when a hook was actually installed — a project
  imported as a plain public repo URL, or whose owner never completed the
  GitHub connection, has `git_ci == None` and NO hook, so GitHub never notifies
  the platform. (2) `git::spawn_git_poll_reconcile` (leader-only, registered in
  `main.rs`) covers exactly that gap: it polls every git-sourced project's
  tracked-branch HEAD with `git ls-remote` and starts the SAME build the webhook
  would whenever HEAD has advanced past the deployed commit. Both dedup on the
  commit SHA (`CloudState::git_poll_seen`, seeded from the deployed commit) so a
  push deploys exactly once regardless of which trigger sees it first — never
  add a deploy path that bypasses that SHA check. A public repo needs no
  credential; a private repo reuses `git_webhook`'s token resolution
  (`github_app_auth` install token, else node `GITHUB_TOKEN`).

## Process

- Git only via the `gm` skill's git verbs (`git_finalize`, `git_push`, etc.)
  — never raw `git` via a shell, which bypasses the porcelain-clean gate.
- No test files, ever — no `test/`/`__tests__/`/`spec/` directories, no new
  `#[cfg(test)]` modules for verifying a change. Verify behavior via the
  existing test suite (`cargo test --workspace`) plus real, live execution
  (curl against a running node, SSH to a fleet node) — never a mock standing
  in for a real service.
- **A local (macOS) `cargo check` does NOT compile `cfg(target_os = "linux")`
  code — it silently skips it and still reports success.** Anything behind a
  Linux `[target.'cfg(...)'.dependencies]` block or a `#[cfg(target_os =
  "linux")]` item is therefore UNVERIFIED by a green local check, and the fleet
  is entirely Linux, so that is production code proved by nothing. Witnessed
  2026-07-31: a clean local check on the jemalloc heap-profiling work, then an
  immediate `E0308` on the first real node build (the dependency resolved two
  incompatible `pprof_util` versions). Build Linux-gated changes on an actual
  node before believing them — the same discipline the no-mocks rule above
  already demands, applied to the compiler.
- **`cargo check` also does not build TEST targets — `#[cfg(test)]` code is
  invisible to it on every platform.** Adding a field to a widely-constructed
  struct therefore passes a green `cargo check --workspace` and then fails CI,
  which runs `cargo test --workspace`. Witnessed 2026-07-31: two new
  `NodeInfo` fields (`disk_free_gb`, `gpu_free_mb`) broke the struct literals
  in `schedule.rs`/`dnsserver.rs` test modules with `E0063`, red across three
  CI jobs, after a clean local check. Before pushing a change to any shared
  struct, run `cargo test --workspace --no-run` — it compiles the test targets
  without executing them, which is exactly the gap `check` leaves.
  Corollary for the two new fields specifically: `0`/`None` mean UNKNOWN, so a
  fixture left at 0 exercises the admit-unknown path, not the has-capacity
  path — set a real value when the test means "this node has space".
- Fleet has two glibc groups needing separate native builds — **2.38**: bkk,
  hk, AND all five GPU/CVM nodes (fc-sanjose-gpu-1/2/3, fc-sanjose-cvm-1/2 —
  TencentOS);
  **2.39**: va/va2/va3/sj/sj2/sp/fr. The GPU nodes LOOK like the San Jose 2.39
  group by region and were once rolled a 2.39 binary on that assumption —
  instant fleet-wide crash-loop on that node (`GLIBC_2.39 not found`, restart
  counter 120+) until the `.old` binary was restored. Membership is by OS
  image, not region. Always sha256-verify + `.old`-backup a binary before
  swapping. Do not hand-maintain this list from memory —
  `scripts/audit-runtime-versions.sh` prints each node's live glibc alongside
  its runtime versions, and is the check to run before ANY binary rollout
  (it is how sp/fr were found missing from this very list).
  Detail: `recall("fleet-glibc-groups")`.
- The dashboard's `/ops/*` proxy forwards every admin request to the CURRENT
  control-plane leader, not the node running the dashboard process — verify
  new admin endpoints through the real dashboard. Detail:
  `recall("ops-proxy-leader-forward")`.
- `scripts/shadw-watchdog.sh` is a PERSISTENT KeepAlive loop (`WATCHDOG_LOOP=1`,
  never StartInterval); needs one manual `launchctl kickstart` after any plist
  re-bootstrap, and its `ensure()` targets track the current `dev.shadw.fc-lax*`
  labels. Detail: `recall("launchd-pended-spawn-gotcha")` /
  `recall("fc-lax2-watchdog-incident")`.
- The backend (`hive-cloud`, systemd `hive-node`) and the dashboard (`ui/`,
  systemd `hive-ui`) are deployed independently — a backend-only fleet
  rollout does NOT ship a `ui/` change. Use `scripts/deploy-ui-fleet.sh`
  for any `ui/`-touching change. Detail: `recall("ui-deploy-gap-incident")`.
- **Third-party runtime versions drift silently and are a real CVE surface.**
  Nothing in the platform pins or audits `firecracker`/`crun`/`runsc`, so
  nodes provisioned at different times sit on different versions
  indefinitely. Witnessed 2026-07-31: firecracker ranged v1.13.0 → v1.17.0-dev
  fleet-wide, leaving bkk/hk/va/va2 inside the affected range for
  CVE-2026-5747 (virtio-pci OOB write, **guest→host RCE** — the escape a
  multi-tenant host most needs to not have) and CVE-2026-1386 (jailer symlink
  → arbitrary host file overwrite). Audit with a one-liner across the fleet
  (`firecracker --version`, `crun --version`, `runsc --version`) whenever a
  runtime CVE lands, and re-check after ANY node bring-up — a freshly
  provisioned node inherits whatever version its role happened to fetch that
  day, which is how the spread opened in the first place. Verify a claimed
  CVE's affected RANGE before upgrading: the same audit showed crun 1.17/1.27
  fleet-wide against a 1.19–1.26 advisory, i.e. no exposure and no upgrade
  warranted — patching on the advisory's headline alone would have been pure
  churn.

@.gm/next-step.md
