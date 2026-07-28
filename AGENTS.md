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
  hk, AND all five GPU/CVM nodes (fc-gpu-sj-1/2/3, fc-cvm-sj-1/2 — TencentOS);
  **2.39**: va/va2/va3/sj/sj2. The GPU nodes LOOK like the San Jose 2.39 group
  by region and were once rolled a 2.39 binary on that assumption — instant
  fleet-wide crash-loop on that node (`GLIBC_2.39 not found`, restart counter
  120+) until the `.old` binary was restored. Membership is by OS image, not
  region. Always sha256-verify + `.old`-backup a binary before swapping.
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

@.gm/next-step.md
