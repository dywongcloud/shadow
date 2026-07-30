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
(`dywongcloud/pvm-no-fsgsbase-rdtscp`: `patches/pvm6.12to7.1complete.patch` is
the PVM series itself and applies onto a **vanilla** tree — that repo's
`kernel/` dir is the old 6.12.33 base, not the build target).

- **`pti=off` is REQUIRED.** With PTI active `kvm_pvm` refuses to load
  ("Support for host KPTI is not included yet") and `/dev/kvm` silently
  disappears, taking Firecracker with it. Fleet convention (see fc-virginia).
- **`make install` RESETS grubby args** — re-apply and re-verify `pti=off
  panic=5` after *every* kernel install, not just the first.
- **`x86_64_defconfig + KVM_PVM + X86_FRED` is nowhere near enough to host
  containers or microVMs.** That minimal config cost four rebuild cycles to
  discover; enable all of:
  - storage/net: `OVERLAY_FS`, `FUSE_FS`, `BRIDGE`, `VETH`, `TUN` (TUN also
    gates Firecracker TAP networking)
  - NAT: `NETFILTER_ADVANCED` (off hides the symbols entirely), plus — new
    symbol splits in 7.1 — `IP_NF_IPTABLES_LEGACY` and
    `NETFILTER_XTABLES_LEGACY`, which `IP_NF_NAT`/`IP_NF_FILTER` now depend on.
    Without them netavark fails with "Module ip_tables not found".
  - containers: `USER_NS` (crun dies on `/proc/self/uid_map`), `MEMCG` (the
    platform sets `HIVE_CONTAINER_MEMORY`; cgroup2 has no memory controller
    without it), `FANOTIFY`
  - cgroup2 device controller: `BPF_SYSCALL`, `CGROUP_BPF`, `BPF_JIT` — else
    crun fails "bpf create: Function not implemented"
  - Firecracker vsock: `VSOCKETS`, `VHOST_VSOCK`; plus `VFIO`/`VFIO_PCI`
- `CONFIG_X86_FRED=y` is required on 7.1 even without FRED hardware, and
  vendor KVM (`KVM_INTEL`/`KVM_AMD`) must be off — PVM replaces them.
- Verify functionally, not by device-node existence: open `/dev/kvm` and do a
  real `KVM_CREATE_VM` + `KVM_CREATE_VCPU`, then actually run a container.

## Bringing a node into the mesh

- **Seed `HIVE_BOOTSTRAP_PEERS` with ADDRESSED peers, never bare node ids.** The
  format (`hive_p2p::parse_seed_addr`) is
  `<64hex-id>[@ip:port[+ip:port]][|relay-url]`; a bare id is accepted but then
  cold-start rendezvous depends entirely on n0/Seer discovery resolving it,
  which is not reliable. Witnessed: three nodes with bare-id seeds served
  healthz 200 and opened 25 mesh trunks, yet each saw ONLY ITSELF in
  `/v1/nodes` while the rest of the fleet couldn't see them either — invisible
  to the dashboard, to placement, and to DNS. Relay-addressed seeds
  (`<id>|http://<public-ip>:3340`) converged the registry immediately.
- `peer_iroh.json` stores each peer's **private** addrs (10.x/172.16/192.168),
  so copying a populated peer book from one node to seed another does not give
  a dialable address — use public IPs / relay URLs for seeds.
- A node's own trust drop-in must list EVERY fleet id (`HIVE_TRUSTED_NODE_IDS`
  + `HIVE_PEER_TRUST=1`), and so must every existing node's — gossip is
  non-transitive and the trust list is an allowlist.
- Health probes run over **iroh**, not HTTP, so a restrictive cloud security
  group that blocks 8786/8787 does NOT by itself make a node unhealthy — but it
  does stop HTTP admin dispatch, which pushes deploys onto the iroh path.

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
  `NodeInfo::dns_ns` (set at boot ONLY for a real public `:53` bind — the dev
  default `127.0.0.1:5354` must never be advertised) is a necessary condition
  and never a sufficient one: it is read out of the node's own env and says
  nothing about reachability. Publishing on it alone put two dead nameservers
  into the live delegation — `ns-fc-hongkong` unreachable (inbound `:53`
  dropped upstream of the host, invisible from on it) and `ns-fc-sanjose-cvm-2`
  answering authoritatively with ZERO records. `dns_probe` closes it: EVERY
  node (not leader-only — the value is independent vantages) queries every
  peer's public `:53` over the public internet, requires `NOERROR` + `AA` +
  ≥1 address record, and gossips the passers as `NodeInfo::dns_attest`.
  `validate_nameservers` then admits a node only while attested from
  **two distinct REGIONS** (a same-datacenter peer is exactly the vantage most
  likely to sit inside whatever still permits the traffic), degrading to one
  only when the fleet has no second region. Never self-attest. Withdrawal is
  damped by 2 consecutive failed rounds, same K as host-health damping.
  Because "responds" is not the bar, the probe also asks on behalf of a
  ROTATING sample of the real client subnets `GeoCache` has located — the
  cvm-2 defect is client-location specific (measured live: 8 records for one
  client, 0 for another), so a probe that only asks on its own behalf is blind
  to the whole class. It samples; it does not prove-for-all-clients.
- **Below two PROVEN nameservers the reconciler HOLDS, it does not withdraw.**
  `desired_geo_delegation` returns no records AND no managed names, so the diff
  never treats the delegation as its own and an already-published NS set is
  left exactly as it is — deleting every NS would turn a degraded delegation
  into a blackholed zone. The hold is loud: `geo_delegation_holds` counts it,
  every pass logs it, and the transition INTO the hold opens an incident (edge
  triggered — `incidents::open` does not dedup).
- **Rollout property to expect:** attestations arrive empty from pre-upgrade
  peers, which withholds advertisement rather than granting it. A fleet where
  too few nodes run the prover therefore HOLDS the existing delegation until
  two regions' worth of provers are up — that is the designed direction of
  failure, not a regression.
- `hive-cloud --dns-probe <ip>[,<ip>…]` runs the SAME probe from any host
  (laptop, bastion, peer) and exits non-zero on failure —
  `HIVE_DEPLOY_ZONE` picks the zone, `HIVE_DNS_PROBE_SUBNETS` adds client
  subnets. Answering "is this nameserver serving?" with a second
  implementation is how the diagnostic and the decision quietly diverge.
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
