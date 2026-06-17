# hive-rs

A Rust reverse-engineering of **Hive**, Vercel's builds infrastructure
([blog post](https://vercel.com/blog/a-deep-dive-into-hive-vercels-builds-infrastructure)).

It reproduces Hive's components — Control Plane, per-Hive API, Box Daemon, Cell
Daemon, warm pools, scheduler, autoscaler, and the cell lifecycle — behind a
pluggable isolation backend:

- **mock** — a cell is a sandboxed child-process build. Runs anywhere (incl.
  macOS / Apple Silicon) so you can exercise the whole control plane today.
- **firecracker** — a cell is a real aarch64 Firecracker microVM on KVM,
  intended to run inside a **Lima** VM with nested virtualization on an M3/M4.

See [ARCHITECTURE.md](./ARCHITECTURE.md) for the concept→code map and the build
lifecycle.

The platform is two layers that share one isolation backend (`CellBackend`):

- **Hive** (builds) — turn a git repo into build output.
- **Fluid** (serving) — deploy static assets + functions, served with **Fluid
  compute**: long-lived instances that each handle many concurrent requests,
  stay warm, autoscale, and scale to zero.

## Layout

```
crates/
  hive-core         shared types: ids, jobs, lifecycle FSMs, wire/agent protocol
  hive-backend      CellBackend trait + mock + firecracker + function-serving
  hive-controlplane build scheduler, warm pool, autoscaler, lifecycle
  hive-api          per-Hive HTTP API (submit / status / stream logs)
  hive-cell-agent   the cell daemon (in-guest; build runner + function bridge)
  hived             a Hive build node
  hivectl           build CLI

  fluid-core        deployment / function / route model (fluid.json)
  fluid-tunnel      one multiplexed tunnel per instance (stream-id framing, metrics, nack)
  fluid-compute     the Fluid pool: in-function concurrency, autoscale, scale-to-zero
  fluid-gateway     public router: static serving + function proxy over tunnels
  fluidd            serving daemon (gateway + pool + admin API)
  fluidctl          deploy CLI
  hive-p2p          distribute the infra over iroh QUIC peer-to-peer
examples/hello/     a deployable app (static page + Python function)
scripts/            Lima config + guest bootstrap for the Firecracker path
```

## Quick start (mock backend — works on your Mac now)

```bash
cargo build

# Start a Hive node with a warm pool of 2 "node:20" cells.
RUST_LOG=info,hive_controlplane=debug \
  ./target/debug/hived --warm node:20=2 --listen 127.0.0.1:8080 &

# Watch the cluster (warm pool fills in the background).
./target/debug/hivectl status

# Submit a build that lands on a warm cell and follow its logs.
./target/debug/hivectl submit --image node:20 \
  -c 'echo hello from the cell' -c 'uname -a' --follow
```

You'll see a warm-pool hit report a `provision_latency` of a few milliseconds,
versus hundreds for an image with no warm pool — the same lever Hive uses to
turn a ~90s cold provision into a ~5s start.

### Build cache (faster repeat builds)

Like Netlify/Hive's shared cache + overlay filesystem, builds can restore/save
directories keyed by a cache key (e.g. a lockfile hash) — so an unchanged
`package.json` skips `npm install`:

```bash
hivectl submit --image node:20 --cache-key "$(shasum package-lock.json)" \
  --cache-path node_modules \
  -c 'test -d node_modules || npm ci' -c 'npm run build' --follow
```

First build is a cache miss (installs + saves); the next build with the same key
restores `node_modules` into the fresh single-use cell before the build runs.

### Try it

```bash
cargo test                      # 8 build + Fluid tests (lifecycle, pool, cache, cost)
hivectl submit --help           # all submit flags (resources, env, repo, cache, ...)
hivectl status                  # boxes, cells, jobs
```

## Deploy like Vercel — Fluid compute (works on your Mac now)

```bash
cargo build

# Start the serving daemon (public :8787, admin :8786).
./target/debug/fluidd &

# Deploy the example app (a static page + a Python function).
./target/debug/fluidctl deploy examples/hello

# Static route:
curl localhost:8787/

# Function route (proxied to a long-lived server in a cell):
curl localhost:8787/api/hello
#  -> {"msg":"hello from a Fluid function","pid":12170,...}
#  response header  x-fluid-instance: cell-c981d91d   (which instance served it)
```

### The Fluid behaviors (all verified)

`fluid.json` declares the knobs: `max_concurrency` (requests per instance),
`min_instances` (keep-warm), `max_instances` (ceiling), `idle_ttl_secs`
(scale-to-zero).

- **In-function concurrency + autoscale** — 8 concurrent slow requests against a
  function with `max_concurrency=5, max_instances=3`:
  ```
  responses per instance:
     5 cell-c981d91d   <- one instance handled 5 concurrent requests
     2 cell-f6064604   <- pool autoscaled to 3 instances for the overflow
     1 cell-092ce497
  ```
- **Reuse before provision** — requests route to the least-loaded running
  instance; warm hits return in ~3ms with no cold start.
- **Scale-to-zero** — idle instances drain back to `min_instances` after
  `idle_ttl` (set `min_instances: 0` to go fully to zero).
- **Streaming responses** — the gateway streams bytes as the function emits them
  (LLM-style token streaming), no buffering:
  ```bash
  curl -N localhost:8787/api/stream   # SSE chunks arrive ~200ms apart
  ```
- **`waitUntil` background work** — a function can respond immediately and keep
  working before its connection closes; the client isn't blocked, and the
  instance stays accounted as active:
  ```bash
  curl localhost:8787/api/bg          # returns in ~3ms; instance works ~0.6s more
  ```
- **Active-CPU cost metering** — Fluid bills shared instance-time, not per-request
  wall time. The meter reports the savings vs traditional 1:1 serverless:
  ```bash
  fluidctl stats   # traditional_ms vs fluid_ms vs savings_pct
  ```
  Under concurrency (5 requests sharing one instance), this is ~80% — the "up to
  85%" story from Vercel's writeups.
- **One multiplexed tunnel per instance** — each instance is reached over a
  single persistent connection that carries many concurrent requests (stream-id
  framing), plus in-band metrics and `nack`. Proven by deterministic concurrency
  tests (`fluid-tunnel` 200-concurrent, `fluid-gateway` 100-concurrent
  end-to-end). Reuse stats at `GET :8786/tunnels`; responses carry
  `x-fluid-reused`.
- **Peer-to-peer infra (iroh)** — the same tunnel runs over an iroh QUIC P2P
  connection, so an instance anywhere is reachable by endpoint id:
  ```bash
  cargo run -p hive-p2p --bin p2p-demo
  #  request 0: status=200 body={"served_over":"iroh-p2p", ...}   ← routed P2P
  #  OK: 5 requests routed over an iroh P2P tunnel
  ```
- **Self-healing routing** — a health-probe loop reaps unreachable instances,
  and if a request hits a dead instance the gateway marks it dead (`nack`) and
  **reroutes** to a healthy/new one. Verified by killing an instance mid-flight:
  ```
  request #1  x-fluid-instance: cell-efd9b1ff   (pid 65228)
  <kill the function process>
  request #2  x-fluid-instance: cell-2e6a17aa   x-fluid-rerouted: 1   (pid 65308)  ✅ still 200
  stats: dead_reaped 1
  ```

Inspect live: `fluidctl stats` and `fluidctl ls`.

This is the faithful Fluid model from Vercel's writeups: instances are
"serverless servers" that multiplex concurrent requests, the router prefers
reusing existing instances and cold-starts only on saturation, and idle capacity
is reclaimed. (Routing here opens one connection per request for simplicity;
real Fluid keeps persistent TCP tunnels and routes by function id for >99%
connection reuse.)

## Real microVMs (firecracker backend, on an M3/M4)

Requires macOS 13+ on an M3/M4 (hardware nested virtualization) and
[Lima](https://lima-vm.io) ≥ 1.0.

```bash
# 1. Boot an aarch64 Linux VM that exposes /dev/kvm to the guest.
limactl start --name=hive ./scripts/lima-hive.yaml
limactl shell hive

# 2. Inside the guest: install firecracker, fetch a kernel, build binaries,
#    and bake a default rootfs (with the cell agent as init).
bash ~/<path-to-repo>/hive/scripts/bootstrap-guest.sh

# 3. Run a real Hive (cells = Firecracker microVMs) and submit a build.
sudo RUST_LOG=info hived --backend firecracker --warm default=2 &
hivectl submit --image default -c 'uname -a' -c 'cat /etc/os-release' --follow
```

Here each cell is a genuine microVM: the box daemon spawns `firecracker`,
configures it over its REST API, and talks to `hive-cell-agent` (the cell
daemon) over **vsock**.

### Verified on real hardware

This was run end-to-end on an **Apple M3 Pro** (macOS 26.3, Lima 2.1, nested
virtualization) booting **real Firecracker v1.13 microVMs**:

```
$ hivectl submit --image default -c 'uname -a' -c 'echo PID1=$(cat /proc/1/comm)' --follow
» [cell-412f4681] connected to cell agent; dispatching build
$ uname -a
Linux (none) 6.1.102 #1 SMP aarch64 GNU/Linux     # guest kernel, not the 6.8 host
$ echo PID1=$(cat /proc/1/comm)
PID1=hive-cell-agent                              # our cell daemon is the VM's init
job → Succeeded (provision_latency=6005ms)        # cold: full microVM boot
```

The warm pool reproduces Hive's headline number in miniature — same image,
pre-booted cell:

| Path | provision latency |
| --- | --- |
| cold (boot microVM on demand) | **6005 ms** |
| warm (pre-booted pool)        | **1 ms** |

Gotchas worth knowing if you reproduce it:
- The per-cell run dir **must be disk-backed**, not tmpfs — `/run` is tmpfs and
  too small for the rootfs copy (`hived --fc-run-dir /var/lib/hive/run`).
- Boot args need `root=/dev/vda rw` (Firecracker exposes the rootfs as `/dev/vda`).
- Firecracker's API keeps the HTTP connection open, so the client parses the
  response by `Content-Length` rather than reading to EOF.
- The rootfs must be glibc-based (Ubuntu/Debian) to match the dynamically-linked
  agent — not Alpine/musl.

## hived flags

```
--backend mock|firecracker      isolation backend (default mock)
--listen ADDR                   API bind address (default 127.0.0.1:8080)
--boxes N                       number of boxes (default 2)
--box-vcpus N / --box-mem-mib N per-box capacity
--warm IMAGE=COUNT              warm-pool target per image (repeatable)
--max-concurrent N              max concurrent builds
--mock-provision-ms N           (mock) simulated cold-boot latency
```
# hive
