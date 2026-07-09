# shadw.cloud

**shadw.cloud** is a self-hosted, peer-to-peer cloud: deploy serverless
functions, containers and static sites to a mesh of your own machines, connected
over **Iroh QUIC**. No data center, no public IPs — nodes find each other and
serve the world.

Under the hood it's a Rust reverse-engineering of **Hive** (Vercel's builds
infrastructure, [blog post](https://vercel.com/blog/a-deep-dive-into-hive-vercels-builds-infrastructure))
plus **Fluid** compute, wired into one node binary and a Vercel-style dashboard.

It reproduces Hive's components — Control Plane, per-Hive API, Box Daemon, Cell
Daemon, warm pools, scheduler, autoscaler, and the cell lifecycle — behind a
pluggable isolation backend:

- **mock** — a cell is a sandboxed child-process build. Runs anywhere (incl.
  macOS / Apple Silicon) so you can exercise the whole control plane today.
- **firecracker** — a cell is a real Firecracker microVM. It runs anywhere a KVM
  interface is available: an **M3/M4 Mac** via a Lima nested-virt VM, a **bare-metal
  Linux box** with real `/dev/kvm`, or even an **ordinary cloud VM without nested
  virtualization** via **PVM** (see [Firecracker without KVM (PVM)](#firecracker-without-kvm-on-plain-cloud-vms-pvm)).

See [ARCHITECTURE.md](./ARCHITECTURE.md) for the concept→code map and the build
lifecycle.

shadw.cloud is two layers that share one isolation backend (`CellBackend`):

- **Hive** (builds) — turn a git repo into build output.
- **Fluid** (serving) — deploy static assets + functions, served with **Fluid
  compute**: long-lived instances that each handle many concurrent requests,
  stay warm, autoscale, and scale to zero.

## The unified cloud (the `hive-cloud` node + dashboard)

Everything is wired into **one node binary** (the `hive-cloud` crate) you can run
across machines as a single shadw.cloud, fronted by a **Vercel-style dashboard**.

```bash
# 1) Run a node (builds + Fluid serving + edge + cron + workflows + regions)
cargo run -p hive-cloud -- --region sfo1 --name node-a   # public :8787, admin :8786

# 2) Deploy an app (static + Python function)
./target/debug/fluidctl deploy examples/hello            # FLUID_ADMIN=http://127.0.0.1:8786

# 3) Run the dashboard (Next.js + Tremor, looks like Vercel)
cd ui && npm install && npm run dev                       # http://localhost:3000

# 4) Add another MacBook to the same cloud (different region)
cargo run -p hive-cloud -- --region iad1 --name node-b --peer http://<mac-a-ip>:8786
```

One node serves, at `:8787` (public) and `:8786` (admin), the full surface:

| Vercel feature | Here |
| --- | --- |
| Builds + Sandbox | Hive control plane + `POST /v1/sandbox` (run code in a cell) |
| Fluid compute | multiplexed-tunnel instances, autoscale, scale-to-zero, cost meter |
| In-function concurrency | many concurrent requests per instance (reuse before provision) |
| Concurrency scaling | per-region **burst limit** (1000/10s) → `503 FUNCTION_THROTTLED`; plan caps 30k/100k (`/v1/concurrency`) |
| Max duration | per-function `max_duration_secs` (Vercel default 300s) → 504 on over-budget |
| Error isolation | one failing/over-budget request never takes down others on the instance |
| WAF | managed SQLi/XSS/traversal signatures + custom rules (`/v1/waf`) |
| Bot management | UA classification, allow good / block bad (`/v1/bot`) |
| CDN | edge cache, states **`x-hive-cache: HIT/MISS/STALE/REVALIDATED`**, **stale-while-revalidate**, header precedence `Vercel-CDN-Cache-Control` > `CDN-Cache-Control` > `Cache-Control` |
| Routing | redirects (308) + rewrites before cache (`/v1/routing`) |
| Cron | scheduled function invocations (`/v1/cron`) |
| Workflows | durable multi-step runs (`/v1/workflows`) |
| Previews | every deployment gets `<deployment-id>.localhost` |
| Regions | `--region` label + multi-node HTTP-gossip mesh (`/v1/nodes`) |
| P2P | `hive-p2p` runs the function tunnel over iroh QUIC |
| Observability | live edge event log (`/v1/logs`) + Overview analytics |

Edge request pipeline (per request, mirroring Vercel's CDN layering):
**routing (redirects/rewrites) → firewall (WAF + bots) → concurrency admission →
CDN cache (HIT/STALE/MISS + SWR) → compute**, tagged with `x-hive-region`.

Verified live end-to-end: `/old`→**308**, `/blog/x` rewritten to the function,
`/api/cached` **MISS→HIT**, SQLi & sqlmap UA → **403**, 15 concurrent vs
burst-limit 5 → **3×200 / 12×503 FUNCTION_THROTTLED**, over-budget request →
**504** while normal requests keep returning 200 (error isolation), sandbox runs
code, cron fires on schedule. (Docs studied: concurrency-scaling, fluid-compute,
how-vercel-cdn-works.)

The dashboard (`ui/`) proxies `/cloud/*` to a node's admin API and has pages for
Overview, Deployments, Functions, Regions, Firewall, Cron, Workflows,
Observability, and Sandbox — dark/Geist, shadcn-style components, Tremor charts.

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
  hive-edge         WAF, bot management, CDN cache, cron, workflows, regions
  hive-cloud        the unified node binary (one cloud across MacBooks)
ui/                 Vercel-style dashboard (Next.js + TypeScript + Tremor)
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

## Public ingress over real DNS (shadw.cloud / *.shadw.app)

Public ingress is plain DNS + self-terminated TLS (`HIVE_INGRESS=dns`, the
default — ngrok is fully retired; see `RUNBOOK.md` for the migration history):

- **`shadw.cloud`** (and `api.` / `admin.`) — the platform: dashboard, developer
  API, ops console. Round-robin A records across every public fleet node.
- **`*.shadw.app`** — deployments: a deployment routes by its **subdomain**
  (the first host label), so `https://my-app.shadw.app/` reaches the gateway
  and serves the `my-app` deployment exactly like `http://my-app.localhost:8787/`
  does locally.

Every node terminates TLS itself (ACME DNS-01 wildcard via `acme.rs`) and runs
the same edge: whichever node DNS hands the request to inspects the `Host`,
looks up the deployment's owning node in the gossip registry, and either serves
it locally or transparently proxies it over the iroh QUIC mesh (with
region-aware failover). Net effect: **hit any node, reach any deployment** —
and ingress has no single point of failure. The authoritative DNS server
(`dnsserver.rs`) answers health-aware A/AAAA for the deploy zone, and the
Vercel DNS reconciler (`vercel_dns.rs`) keeps the platform zone's records in
sync with fleet health.

Preview-unlock redirects detect a public wildcard host and bounce to the public
dashboard origin (`HIVE_PUBLIC_DASHBOARD_URL`, `https://shadw.cloud`) rather
than `localhost`.

## Firecracker without KVM on plain cloud VMs (PVM)

Firecracker is a Type‑2 VMM: it asks the host's **KVM** subsystem (`/dev/kvm`) to
create a VM, and KVM in turn programs the CPU's hardware virtualization extensions
(Intel **VT‑x**/`vmx` or AMD‑V/`svm`). On bare metal those extensions are present;
on an Apple Silicon Mac, Lima's `vz` VM enables **nested virtualization** so the
guest sees them too. But a stock cloud VM almost never exposes nested virt — the
hypervisor hides VT‑x/AMD‑V from your instance — so KVM has nothing to initialize,
`/dev/kvm` never appears, and Firecracker can't start.

You can see exactly that on our Virginia node, an AMD Tencent CVM: **no hardware
virtualization is exposed to the guest, yet `/dev/kvm` exists anyway.**

```text
$ grep -c -E 'svm|vmx' /proc/cpuinfo      # AMD‑V / VT‑x available to this VM?
0                                          #   → none. No nested virt.
$ lsmod | grep kvm
kvm_pvm   53248  4
kvm      1404928  1 kvm_pvm                #   → kvm_amd is NOT loaded; kvm_pvm is.
$ ls -l /dev/kvm
crw-rw-rw- 1 root kvm 10, 232 /dev/kvm     #   → the KVM device is present regardless.
```

### What PVM is, and how it conjures `/dev/kvm`

**PVM (Paged Virtual Machine)** is a *software* hypervisor (from loophole labs /
the upstream Linux PVM project) that runs guests **without any hardware
virtualization support**. Instead of trapping into VT‑x/AMD‑V root mode, PVM runs
the guest **paravirtualized**: a cooperating, PVM‑aware guest kernel runs
de‑privileged, and privileged operations (page‑table changes, mode switches,
hypercalls) are mediated by the host. Crucially, PVM is implemented as a **KVM
"vendor" backend** — a kernel module (`kvm-pvm.ko`) that plugs into the generic
`kvm` core exactly where `kvm-intel`/`kvm-amd` normally would:

```text
$ modinfo kvm_pvm | egrep 'filename|depends'
filename: /lib/modules/6.12.33-pvm+/kernel/arch/x86/kvm/kvm-pvm.ko
depends:  kvm
```

Because it sits behind the same `kvm` core, **it exposes the identical `/dev/kvm`
ioctl ABI** (`KVM_CREATE_VM`, `KVM_CREATE_VCPU`, `KVM_RUN`, memory‑region and CPUID
ioctls, …). That is the whole trick: any VMM that speaks KVM — QEMU, cloud‑hypervisor,
**Firecracker** — keeps working, because from userspace `/dev/kvm` looks and behaves
like real KVM. PVM just satisfies those ioctls in software + paravirt instead of
with silicon.

### The two-kernel split (why a stock guest kernel won't boot)

PVM requires cooperation on **both** sides, so two different kernels are involved:

- **Host kernel** — a PVM‑patched kernel (ours: `6.12.33-pvm+`) that provides the
  host side and loads `kvm` + `kvm-pvm`. Once it's booted, `/dev/kvm` is available
  even though `/proc/cpuinfo` shows neither `svm` nor `vmx`.
- **Guest kernel** — must be **PVM‑aware**. A normal `vmlinux` (or a Firecracker‑CI
  kernel) will *not* boot under PVM, because the guest has to drive PVM's
  paravirt interface rather than expect hardware virt. Ours is built with:

  ```text
  CONFIG_PVM_GUEST=y        # the PVM paravirt guest port
  CONFIG_PARAVIRT_XXL=y     # full paravirt-ops (MMU, CPU, IRQ) the guest hooks into
  CONFIG_HYPERVISOR_GUEST=y
  CONFIG_KVM_GUEST=y
  ```

  This is why each microVM boots our `vmlinux-pvm-guest` image, not the generic
  kernel we use on hardware‑KVM nodes.

### The Firecracker fork

We run the **PVM fork of Firecracker**
([loopholelabs/firecracker @ `main-live-migration-pvm`](https://github.com/loopholelabs/firecracker/tree/main-live-migration-pvm),
`v1.13.0-dev`). Because `/dev/kvm` is preserved, the vast majority of Firecracker
is untouched; the fork carries the PVM‑specific bits of vCPU/CPUID/segment setup
and the live‑migration work the branch is named for. (PVM's design — a fully
software‑defined CPU state — makes microVMs cleanly migratable between hosts.)

### What our node had to do (almost nothing)

Because PVM presents `/dev/kvm`, the Firecracker backend's capability probe —
`is_supported()` = *Linux + `/dev/kvm` + a `firecracker` binary* — **passes with no
change**, so a PVM node auto‑selects the real microVM backend exactly like bare
metal:

```
isolation backend: Firecracker microVM (real, Linux + /dev/kvm)
control plane starting hive=hive-virginia ... backend="firecracker"
```

The only PVM‑specific accommodation is the **guest kernel cmdline**. PVM's emulated
platform stalls if the guest probes the legacy i8042 keyboard controller, so we
disable it — overridable per node via the **`HIVE_FC_BOOT_ARGS`** env var, keeping
`init=/sbin/hive-cell-agent` so the cell agent is PID 1:

```
HIVE_FC_BOOT_ARGS="console=ttyS0 reboot=k panic=1 pci=off \
  i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd \
  root=/dev/vda rw init=/sbin/hive-cell-agent"
```

Everything downstream is identical to the hardware‑KVM path: the per‑deployment
build is delivered into the cell as a **virtio‑blk data disk** (mounted at
`/build`), and the gateway reaches the function through the **vsock** tunnel to the
in‑guest cell agent. End‑to‑end, our Virginia CVM serves deployments from genuine
Firecracker microVMs over PVM.

### Trade‑offs

- **Pro:** real VM isolation (a separate guest kernel, not a shared one like
  containers) on commodity cloud VMs that can't do nested virt — no special
  instance type, no bare metal.
- **Con:** software virtualization has CPU overhead versus hardware KVM, so it's
  best for I/O‑bound / serverless workloads (our case) rather than CPU‑pinned ones.
- **Verify it's actually PVM on any node:** `grep -E 'svm|vmx' /proc/cpuinfo` (empty),
  `lsmod | grep kvm_pvm`, `ls -l /dev/kvm`, `firecracker --version`.

Background reading: Alex Ellis,
["How to run Firecracker without KVM on regular cloud VMs"](https://blog.alexellis.io/how-to-run-firecracker-without-kvm-on-regular-cloud-vms/),
and the [PVM Firecracker fork](https://github.com/loopholelabs/firecracker/tree/main-live-migration-pvm).

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

---

**shadw.cloud** — the peer-to-peer cloud.
