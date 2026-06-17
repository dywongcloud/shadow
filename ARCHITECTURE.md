# Hive-rs — reverse-engineering Vercel's builds infrastructure

A study reimplementation, in Rust, of the architecture described in
[*A deep dive into Hive, Vercel's builds infrastructure*](https://vercel.com/blog/a-deep-dive-into-hive-vercels-builds-infrastructure).

The goal is faithfulness to the **design**, not the proprietary code: the same
component boundaries, lifecycle, and warm-pool strategy, with a pluggable
isolation backend so the exact same control plane drives either sandboxed
processes (anywhere) or real Firecracker microVMs (inside Lima on an M3/M4).

## What Hive is (from the post)

| Hive concept | Role |
| --- | --- |
| **Hive** | A regional cluster with an independent failure boundary. |
| **Box** | A bare-metal machine subdivided into cells; caches Docker images. |
| **Cell** | A microVM with dedicated CPU/mem (rate-limited disk/net). 1 cell ↔ 1 Firecracker process, on KVM. |
| **Control Plane** | Job placement, autoscaling, lifecycle, cluster health. |
| **API** | A minimal per-Hive API for cell-execution requests. |
| **Box Daemon** | Spawns Firecracker, talks to cells over sockets. |
| **Cell Daemon** | Drives the build container *inside* the cell. |
| **Warm pool** | Pre-provisioned cells → cold provision drops from **~90s to ~5s**; ~30% faster builds overall. |

## The Apple-Silicon angle

Firecracker needs KVM, which macOS lacks — *except* M3/M4 added hardware
**nested virtualization**. So we run an aarch64 Linux VM via **Lima**
(`nestedVirtualization: true`), which exposes `/dev/kvm` to the guest, and run
Firecracker natively inside it. The Rust system runs in that Linux layer.

```
macOS (M3/M4)
└── Lima VM  (aarch64 Linux, nestedVirtualization=true, /dev/kvm present)
    └── hived  (one process plays Control Plane + API + Box Daemon)
        ├── warm pool of pre-provisioned cells
        └── cell = 1 Firecracker microVM (KVM)
            └── hive-cell-agent  (the Cell Daemon, PID1 / init, vsock server)
```

## Concept → code map

| Hive concept | Where it lives |
| --- | --- |
| `Hive`, `Box`, `Cell`, `Job` ids | `hive-core::ids` |
| `BuildJob`, `ResourceSpec` | `hive-core::job` |
| Cell & Job lifecycle state machines | `hive-core::state` |
| Wire messages (API + agent protocol) | `hive-core::proto` |
| **Control Plane** (scheduler/autoscaler/lifecycle) | `hive-controlplane::Hive` |
| Warm pool + autoscaler reconcile loop | `hive-controlplane`: `reconcile_warm_pool`, `provision_warm` |
| Placement / scheduling | `hive-controlplane`: `decide_one`, `schedule_pass`, `pick_box`, `take_warm` |
| Box capacity ledger | `hive-controlplane::records::BoxRecord` |
| Per-job log fan-out (replay + live) | `hive-controlplane::logbus::LogBus` |
| **API** (per-Hive HTTP ingress) | `hive-api` |
| Isolation contract (`CellBackend`) | `hive-backend` |
| Mock backend (process sandbox) | `hive-backend::mock` |
| **Firecracker** backend (Box-Daemon side) | `hive-backend::firecracker` |
| **Cell Daemon** (in-guest agent) | `hive-cell-agent` |
| `hived` (runs a Hive node) / `hivectl` (client) | `hived`, `hivectl` |
| Lima + guest bootstrap | `scripts/` |

## Lifecycle of a build

1. **Submit** — `hivectl submit` → `POST /v1/jobs` → control plane enqueues a
   `BuildJob` (state `Queued`).
2. **Place** — the scheduler (`decide_one`, under one lock, never across an
   `.await`) tries, in order:
   - a **warm-pool hit** for the job's image (instant, the fast path); else
   - a **cold provision**: reserve box capacity, create a `Provisioning` cell.
3. **Provision** — outside the lock, the backend boots the cell. For Firecracker
   that's: copy a per-cell rootfs, spawn `firecracker`, configure it over its
   REST-over-unix-socket API (`machine-config`, `boot-source`, `drives`,
   `vsock`), then `InstanceStart`. Warm cells already paid this cost.
4. **Run** — `run_build` connects to the cell daemon over **vsock** (host-
   initiated `CONNECT`), ships the `BuildJob`, and streams `AgentEvent::Log`
   frames to the job's `LogBus`; the agent runs each step and replies `Done`.
5. **Finish** — job → `Succeeded`/`Failed`/`TimedOut`; metrics recorded,
   including `provision_latency_ms` (submit → cell starts work — the number warm
   pools minimize).
6. **Teardown** — cells are **single-use**: terminate the microVM, release box
   capacity, drop the record, wake the scheduler.
7. **Refill** — the autoscaler tops the warm pool back up to target and reaps
   extras / stale cells (TTL).

## Design invariants

- **One lock, never held across `.await`.** All cluster state is in a single
  `parking_lot::Mutex<Inner>`; slow backend work happens outside it. This keeps
  the concurrency model easy to reason about and prevents async deadlocks.
- **Every reservation has one release.** Box capacity is reserved at placement
  (or warm provision) and released exactly once at teardown/failure — covered by
  the `capacity_is_released_after_builds` test.
- **The control plane is backend-agnostic.** It only ever calls
  `provision / run_build / terminate`. Swapping mock ↔ Firecracker changes
  nothing above the trait.

## The Fluid serving layer

Hive builds code; **Fluid** serves it. The two layers share the `CellBackend`
abstraction — a function instance is just a cell that runs a long-lived server
instead of a one-shot build.

Based on Vercel's Fluid writeups ([how it works](https://vercel.com/blog/how-fluid-compute-works-on-vercel),
[serverless servers](https://vercel.com/blog/fluid-how-we-built-serverless-servers)):

| Fluid concept | Where it lives |
| --- | --- |
| Deployment / function / route model (`fluid.json`) | `fluid-core` |
| **In-function concurrency** (many requests per instance) | `fluid-compute`: `max_concurrency`, per-instance `inflight` |
| **Reuse before provision** (least-loaded instance, cold-start on saturation) | `fluid-compute::Fluid::decide_lease` |
| **Keep-warm** (`min_instances`) + **scale-to-zero** (`idle_ttl`) | `fluid-compute::Fluid::reconcile` |
| Functions router (route match → static or function proxy) | `fluid-gateway` |
| **Single multiplexed tunnel per instance** (one connection, many concurrent requests, stream-id framed) | `fluid-tunnel` (`TunnelClient`/`TunnelServer`); gateway keeps one per instance |
| **In-band metrics + `nack`** (instance pushes load; overload rejection) | `fluid-tunnel` `Metrics`/`Nack` frames |
| **Streaming responses** (LLM-style) | `fluid-gateway`: response body streamed via `Body::from_stream`, lease held to EOF |
| **`waitUntil`** background work after response | function keeps the connection open post-response; lease released at EOF |
| **Active-CPU cost metering** ("up to 85%") | `fluid-compute`: `traditional_ms` vs `fluid_ms`, `savings_pct` |
| **Health checks + `nack`/reroute** (instance failure → drop + retry elsewhere) | `fluid-compute`: `mark_dead` + health-probe loop; `fluid-gateway` reroute loop (`x-fluid-rerouted`) |
| **Rust runtime bridge** (env, spawn, stream, waitUntil) | `hive-cell-agent` is the in-cell Rust bridge to the user process |
| Serving daemon + admin API / deploy CLI | `fluidd`, `fluidctl` |

And on the build side (Hive + Netlify):

| Build concept | Where it lives |
| --- | --- |
| **Build cache** (deps/output keyed by lockfile hash) | `BuildJob::cache` + `hive-backend::mock` restore/save |
| **Build cache inside microVMs** | cell agent tars cache paths and ships them to the box daemon over vsock (`AgentEvent::CacheGet/CachePut`); host stores in `FirecrackerConfig::cache_dir` |
| Shared cross-cell cache storage | `MockConfig::cache_root` / `FirecrackerConfig::cache_dir` |
| Pre-warmed VMs / pull-of-work | Hive warm pool + scheduler (`hive-controlplane`) |

## Distributing the infra peer-to-peer (iroh)

The tunnel protocol is transport-agnostic, so it runs over an **iroh** QUIC P2P
connection as easily as over TCP/vsock. `hive-p2p` binds an iroh endpoint
(identified by a public-key endpoint id) speaking a `hive/tunnel/0` ALPN:

| Concept | Where it lives |
| --- | --- |
| Bind a P2P endpoint (relay + DNS discovery) | `hive-p2p::bind` (iroh `N0` preset) |
| Instance side: accept P2P conns, serve tunnels | `hive-p2p::serve_tunnels` -> `fluid_tunnel::TunnelServer` |
| Gateway side: dial an instance by endpoint id | `hive-p2p::dial` -> duplex stream -> `fluid_tunnel::TunnelClient` |
| Demo | `cargo run -p hive-p2p --bin p2p-demo` (routes requests between two endpoints P2P) |

This means a box or instance can run anywhere reachable by iroh (NAT traversal /
relay fallback handled by iroh) and the gateway reaches it by id — no public IPs.

**Request lifecycle (serving):** request → gateway selects deployment (by Host
subdomain, else default) → resolves route → static file *or* `Fluid::lease`
(reuse least-loaded instance with a free slot, else cold-start up to
`max_instances`) → proxy HTTP to the instance over its endpoint (TCP for mock,
vsock for Firecracker) → release the lease. The autoscaler keeps `min_instances`
warm and drains idle instances after `idle_ttl`.

**Faithful selection rule:** among running instances, pick the one with the
*fewest* in-flight requests (Vercel found this beats round-robin), and only
provision a new instance when all are at `max_concurrency` — reuse before
provision.

## Where this is faithful vs. simplified

Faithful: the hierarchy, the cell↔Firecracker 1:1 mapping, the box daemon ↔
cell daemon split over a socket (vsock), warm pools as the latency lever,
single-use cells, autoscaling/reaping, per-job streamed logs.

Simplified (documented, not hidden):
- One `hived` process collapses Control Plane + API + Box Daemon. The seams are
  the `CellBackend` trait and the `proto` messages — the natural cut points to
  split across machines.
- Per-cell rootfs is a **copy** of a base ext4, not copy-on-write.
- Box "Docker image caching" is modeled as warm pools keyed by image + a base
  rootfs per image, rather than a layer cache.
- Disk/network rate-limiting per cell is not enforced (Firecracker supports it
  via rate limiters; wiring is a TODO in the backend config).
- Scheduling is single-Hive, single-region; cross-Hive routing is out of scope.
- Each instance is reached over a single multiplexed tunnel (stream-id framed,
  many concurrent requests) carrying in-band metrics and nack — proven by
  deterministic concurrency tests (`fluid-tunnel` 200-concurrent, `fluid-gateway`
  100-concurrent end-to-end). There is no separate `compute-resolver` service
  (routing is in-process, so sticky-by-function-id is implicit), and the gateway
  also keeps an out-of-band health probe. Everything else from the writeups —
  streaming responses, `waitUntil`, active-CPU cost metering, health/`nack`
  reroute — is implemented.
- Deployments are delivered as a local path (mock backend reads files directly);
  a real deploy would upload a build artifact and bake/mount it into the cell.
- Billing/active-CPU metering is not modeled.
