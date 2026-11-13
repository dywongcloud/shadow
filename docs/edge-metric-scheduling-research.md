# Decentralized kernel-metric scheduling: what it can and cannot do for us

Research into `ajitm722/Decentralized-Kernel-Metric-Driven-Scheduling-for-Edge-Faas-Workloads`
(C/Go, branch `main`, commit `8ed462c35ad8d425d8067e91612d41d85185fa21`, 47 blobs,
no LICENSE). Done with an adversarial swarm: three parent agents, each spawning its
own refuter sub-swarm. Findings below are the RECONCILED positions, not the first
draft — every `refuted` note is a correction that survived.

## What that project actually is (not what its README claims)

- Real eBPF tracepoints (`sched_switch`, `sched_process_exit`,
  `thermal_temperature`), real Docker execution, real 3-device ARM64 hardware
  (Jetson Orin Nano, Pi 5, host). `bpf/cpu_core.c:51,95`,
  `bpf/thermal_core.c:48`.
- Gossip is a 3 s unauthenticated gRPC `Push` of a `MetricsSnapshot` to a static
  `--peers` list; `NodeTTL = 4 s`; `OFFLINE` is a LOCAL TTL check
  (`cmd/peer.go:33,111,355,407,452`). Nothing is propagated. **README:40's
  "failure state is propagated across the mesh" is false** (refuter C3/C8).
- Placement bands: `cpu + req_cpu < 95.0`, `mem + req_mem < 90.0`
  (`cmd/peer.go:120-121`), thermal `HOT > 80 / WARM > 60` (`cmd/tempwatch.go:146`).
  **Thermal is a PRIORITY, not a filter**: with no SAFE candidate, HOT nodes stay
  eligible (`cmd/peer.go:134-152`) — README:427 is documentation, not code.
  Selection is `rand.Intn` plus a `"localhost"` override (`:155-166`).
- `ReqCpu`/`ReqMem` are CLIENT-DECLARED and unenforced; there is no reservation,
  quota, tenancy, authn/authz, TLS, signed message, job-id dedupe, hop/TTL on
  recursive forwarding, container CPU/mem/pids limits, image digest pin, or exit
  code check (`statusCh` discarded, any exit reports "Completed Successfully").
- Evaluation measures ONLY agent co-location overhead (CPU ~0.55% Jetson,
  ~2.57% Pi5; memory -1.17%/+6.32% from asset PNGs, ~10 runs, no significance
  test). **No measurement of offload benefit, latency, deadline misses, or
  thermal throttling.** README:63's "prevent thermal throttling" is unmeasured.

## What OUR codebase has today (verified file:line)

- `crates/hive-cloud/src/schedule.rs:41-50` `load_map` = served host count +
  peer route count. `:313-349` `capable` = GPU, wasm/bun capability, runtime
  artifact protocol, build isolation, build toolchain, disk floor, eligible/
  reachable. `:384-449` `dispatch_fallbacks`, `:457-608` `place`, `:57-126`
  `place_for_project` (lease stickiness).
- **NO live CPU/thermal/memory-pressure term in placement.** Disk is a hard
  filter; unknown admits. Refuter confirmed this and refuted the narrower
  "only those signals" claim: region/no-region branches, `stateful`,
  `is_self`/`exclude`, `is_mock_backend`, `has_public_address` also participate.
- **eBPF/thermal/`hwmon`/PSI/`perf_event` source is ABSENT.** Kernel-derived
  metrics DO exist (cgroup memory pressure / OOM audit, per-cell CPU sampling)
  but are NOT placement inputs; `seccomp-bpf` is classic BPF only. Do not claim
  "no kernel metrics at all".
- Control plane: `HIVE_CP_OWNER_CHAIN` -> `cluster.rs:131-142`;
  `leadership::may_act` -> `leadership.rs:416-418`; mutation forwarding ->
  `main.rs:2241-2248`. **`place`/`dispatch_fallbacks` call no `may_act`:
  scoring and dispatch are NODE-LOCAL.** So "leaderless scheduling contradicts
  our invariants" is overbroad; only independent shared-state writes,
  autonomous leader-only jobs, election fallback while a chain is set, or
  wholesale adoption from a backup would contradict them.
- Staleness is already a first-class concern: `bounded_round::BoundedRound`,
  `health::demote` (withdraw only after `DEMOTE_STALE_ROUNDS`, HOLD while
  stalled), gossip straggler leash keyed by ENDPOINT id.

## Verdict: adopt the OBSERVATION IDEA, reject the SYSTEM DESIGN

| Idea | Verdict | Reason |
| --- | --- | --- |
| Periodic per-node pressure signal (CPU/thermal/mem) | ADAPT, as a RANK not a gate | We have none; a rank cannot empty the candidate set or weaken disk/capability hard filters. Must go through the ONE `capable` predicate shared by `place`, lease stickiness and `dispatch_fallbacks`. |
| Leaderless gossip membership | REJECT | We already have mesh gossip with stale-reading discipline, signed+trusted peers and a strict owner chain. Their 4 s local TTL with no propagation is strictly weaker; authenticated mesh is non-negotiable. |
| Proactive offload / random peer | REJECT | `rand.Intn` + unauthenticated `SubmitJob` + arbitrary image/args = remote code execution. Would violate tenancy, `require_mesh_rpc`, and every "never weaken" gate. Our `dispatch_fallbacks` already retries on `nothing_ran()` with `capable` + `reachable`, deterministically. |
| Thermal as a hard filter | REJECT | Their own code keeps HOT eligible and their paper never measures throttling. A hard filter here could empty placement and HOLD every deploy fleet-wide. |
| Client-declared CPU/mem admission | REJECT | Trivially spoofable (`0,0` always passes); our per-tenant caps are server-derived and store-scoped. |
| eBPF kernel instrumentation | HOLD, operator-gated | Linux-only, needs a reviewed pinned probe, gVisor/PVM kernel support, and a capability advertisement. macOS CI cannot witness it. No digest or kernel ABI is guessed. |

## What would actually help us, in dependency order

1. **Define the measurement, not the scheduler.** A `NodePressure` snapshot
   (cgroup CPU/mem + optional thermal) published through the EXISTING gossip
   `NodeInfo` with `#[serde(default)]`, `None` = UNKNOWN/ADMITTED. It must be
   PROBED on the node and advertised, never inferred from an operator laptop.
2. **Fold it into `build_toolchain_rank`-style ordering** in `place` and
   `dispatch_fallbacks` (and lease stickiness), so a saturated node sorts last
   while remaining admissible. Add a bounded round deadline and a
   stale-reading HOLD, reusing `BoundedRound`/`health::demote` rather than a
   second primitive.
3. **Prove it live** before any claim: assert a saturated node loses a tie-break
   to a healthy peer; assert an unreported/stale node does not win by default;
   assert one stale reading never withdraws a node. Witness with the real node
   and `cargo test --workspace --locked`, never a synthetic test file.
4. **Do NOT** import their code: no LICENSE, unauthenticated gRPC, no tenancy,
   Docker-as-`root`, hardcoded Tegra ABI offsets, no CO-RE. Ideas only.

## Honest limits of this research

- The external repo's own benchmark does not measure the thing we would adopt
  (scheduling benefit). Any speedup or stability claim copied from it would be
  unverified and therefore inadmissible under our measurement gate.
- Leaderless coordination would rewrite `may_act`/owner-chain invariants that
  exist because of real production incidents; this document does not propose it.
- No eBPF program, kernel probe, gossip change, or placement change was written
  here. This is analysis only. The two refuters returned `refuted=true` on
  overbroad claims; the corrections are folded in above.
- Fleet hosts are not administrable from this environment, so no live
  thermal/CPU measurement was taken on the fleet. That remains an operator task.
