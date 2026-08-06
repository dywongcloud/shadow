# `hive_browser_node` — one headless browser node per host

Runs the `/run-node` browser node **unattended on a server**: headless
Chromium loads a loopback page that owns the *shipped, byte-identical*
`ui/public/run-node-worker.js`, which boots `crates/hive-browser`'s wasm
`BrowserNode` over iroh QUIC via a WSS relay, admits against
`POST /v1/browser/admissions`, publishes presence, and serves the tenant's
browser-eligible functions.

Target hosts: **fc-bangkok, fc-sanjose, fc-virginia, fc-saopaulo,
fc-frankfurt** — exactly those five, one node each.

```bash
cd ansible
ansible-playbook playbooks/browser-nodes.yml
```

---

## What actually runs on the host

| Unit | Runs | Holds |
| --- | --- | --- |
| `hive-browser-node-broker.service` | `node scripts/hive-browser-node-broker.mjs` on `127.0.0.1:3009` | the internal token (via `LoadCredential=`), nothing else |
| `hive-browser-node.service` | `scripts/hive-browser-node-run.sh` → headless Chromium → `http://127.0.0.1:3009/` | a short-lived tenant cookie, nothing else |

Everything the browser loads (`/`, `/run-node-worker.js`, `/browser-node/**`,
`/cloud/*`, `/api/token`) is served by the broker on loopback. `127.0.0.1` is a
*trustworthy origin* in every browser, so WebCrypto / IndexedDB / OPFS — which
the worker's identity and db lanes need — all work with no TLS and no public
hostname.

The role does **not** depend on `hive-ui` being installed or running: the
worker and the wasm bundle are pushed straight from the control host's checkout
of `ui/public/`, pinned to this role's run.

---

## (a) Which browser, and why

**Google Chrome stable, from Google's own GPG-signed RPM repo**, run with
`--headless=new`.

* The fleet is TencentOS Server / Rocky 10 — dnf, EL-family. `chromium` there
  means EPEL, which is neither consistently available nor consistently current
  on TencentOS; Google's repo is first-party and signed.
* `--headless=new` is the *full* browser in headless mode. The old
  `headless_shell` build drops pieces this workload actually needs
  (SharedWorker/dedicated-Worker semantics, OPFS, parts of IndexedDB). The
  worker uses all of them.
* `hive_browser_node_chrome_package` / `_chrome_bin` point the role at a distro
  `chromium` instead if you want one; nothing else changes.

**The Chromium sandbox stays on.** The role asserts
`user.max_user_namespaces > 0` and fails loudly (with the exact `sysctl`
remediation) rather than quietly passing `--no-sandbox`. Browser functions are
untrusted tenant JS by design, and this host also runs `hive-node` — this is
the last place to trade a sandbox for convenience. `-e
hive_browser_node_no_sandbox=true` exists as a documented, loud override.

Version drift is a real CVE surface on this fleet (see AGENTS.md), so the role
prints the installed version on every run and uses `state: present`, never
`latest`.

---

## (b) How it authenticates — and why there was only one answer

The worker admits with the httpOnly `hive_jwt` cookie the dashboard's
`/api/token` route mints from a verified **Clerk** session. A server has no
human to sign in. The backend leaves no second door:

`crates/hive-cloud/src/browser_admission.rs::fresh_user_claims` rejects

* `sub` starting with `key:` → **a dashboard API key can never admit**
* `role == "service"` → **no service identity can admit**
* `now - iat > HIVE_BROWSER_SESSION_MAX_AGE_SECS` (default **300s**) →
  **no long-lived credential of any kind can admit**

So the only admissible credential is a *freshly minted platform JWT*, and the
only mint is `POST /v1/token`, gated by
`crates/hive-cloud/src/admin.rs::mint_allowed` on
`x-hive-internal == HIVE_INTERNAL_TOKEN`.

**Decision: a node-local broker mints on loopback; the browser only ever holds
the resulting short-lived cookie.**

Rejected alternatives, and why:

| Alternative | Why not |
| --- | --- |
| Headless Clerk sign-in with a real account | Puts a human's long-lived password/OAuth credential in a unit file or vault for a *robot* — exactly the thing the brief forbids. Also breaks on MFA and bot protection. |
| A dashboard API key (`hive_…`) | Structurally rejected: `sub` is `key:<id>`. |
| A dedicated "service" JWT with a long TTL | Structurally rejected twice: `role == "service"`, and the 300s `iat` freshness bar. |
| Put `HIVE_INTERNAL_TOKEN` in the browser's environment | The internal token can mint *any* tenant. It must never be reachable from the process that executes untrusted tenant JS. |

### What the browser is actually given

The broker hardcodes every claim and **ignores the request body**, so a
compromised page cannot mint anything else:

```
sub    = fleet-browser-node:<hive_name>   auditable, never a human's id, stable
                                          across re-mints (browser_presence's
                                          require_owned_admission compares subject)
tenant = hive_browser_node_team           one tenant, from config, never the caller
role   = member                           may_serve_public needs owner/admin, so a
                                          fleet node cannot hold a PUBLIC admission
email  = ""                               mint_token derives platform_admin from
                                          its OWN admin_emails set against this
                                          field — an empty email is structurally
                                          incapable of producing an operator token
ttl    = backend's 1h, re-minted every 2 min
```

### Where the internal token lives

* On disk: `/etc/hive/browser-node.internal-token`, `root:root 0600`.
* Into the process: systemd `LoadCredential=internal-token:…`, which reads it
  as root and re-exposes it **to the broker unit only** as a `0400` file on a
  private tmpfs.
* Consequently absent from: the unit files, `systemctl show`, `systemctl cat`,
  every `EnvironmentFile`, and `/proc/<pid>/environ` of every process on the
  host — including Chromium's, which is a separate unit and never receives it.
* The broker unit additionally runs under `IPAddressDeny=any` /
  `IPAddressAllow=localhost`: the one process that holds the token has no route
  to the internet at all.

This adds **no new secret to these hosts** — `HIVE_INTERNAL_TOKEN` is already
present on every one of them (`hive-node.service`, `ui/.env.local`). It moves
its custody into the most restricted process on the box.

---

## (c) Resource caps

`hive-node` runs with **no caps at all** (`hive-node.service` sets only
`LimitNOFILE`). Every bound here therefore exists to guarantee the *platform*
process wins any contest for the host.

### cgroup (systemd) — the hard bound

| Directive | Value | Reasoning |
| --- | --- | --- |
| `MemoryMax` | `min(2048, max(768, MemTotal/16))` MiB | Absolute and fact-derived. A flat percentage is wrong at both ends of this fleet: 1/16 of fc-saopaulo's 2.2 TB is 140 GB, and 1/16 of a 4 GB host is 256 MB — below Chromium's own floor, so it would OOM-loop. The role refuses outright if the resulting cap exceeds ¼ of the host's RAM. |
| `MemoryHigh` | 75% of `MemoryMax` | The kernel *throttles and reclaims* at `High` and only *kills* at `Max`. A memory spike costs the browser a stall instead of its admission. |
| `MemorySwapMax` | `0` | A browser node must never push `hive-node`'s pages to swap. It is optional donor capacity; the platform is not. |
| `CPUQuota` | `min(200%, max(50%, vCPU*100/16))` | At most 2 cores' worth, at most 1/16 of the box, never below half a core — under that, a wasm cold start takes minutes and every admission round-trip times out, which is worse than not running. |
| `CPUWeight` | `20` (vs `hive-node`'s default `100`) | The quota bounds an *idle* host; the weight decides a *saturated* one. Both are `system.slice` siblings, so this is a real 5:1 split under contention. |
| `IOWeight` | `20` | Same reasoning for disk. Chromium's profile writes must never delay a deployment checkout. |
| `TasksMax` | `384` | A headless Chromium with one renderer runs ~120–150 threads. Real headroom, no room for a fork bomb. |
| `OOMScoreAdjust` | `700` (`hive-node` is `0`) | **The single most important line.** Under genuine global pressure the kernel kills the highest score first — so the browser dies and the platform lives. |
| `Nice` | `10` | Cheap tiebreaker on the CPU scheduler. |
| `RuntimeMaxSec` | `86400` + `RuntimeRandomizedExtraSec=3600` | Daily recycle bounds long-run renderer growth; randomized so five nodes never restart in the same minute. The gap costs one presence republish, well inside the backend's 90s TTL. |

Broker unit: `MemoryMax=192M`, `CPUQuota=25%`, `TasksMax=32` — it is a ~200-line
loopback HTTP server; a runaway one must also be incapable of disturbing
`hive-node`.

**Worked example** — a 16 GiB / 8 vCPU node resolves to `MemoryMax=1024M`,
`MemoryHigh=768M`, `CPUQuota=50%`, `--max-old-space-size=563`. fc-saopaulo
(2.2 TB / 512 vCPU) clamps to `MemoryMax=2048M`, `CPUQuota=200%` — 0.09% of its
RAM and 0.4% of its cores.

### Chromium's own flags — the soft bound

`--renderer-process-limit=1` (one page ⇒ one renderer),
`--js-flags=--max-old-space-size=<55% of MemoryMax>` so **V8 GCs under its own
pressure before the cgroup OOM-kills the process** (a GC pause is recoverable;
a kill loses the admission), `--disk-cache-size=128MB`, and the usual
server-hygiene set (no component update, no crash upload, no keyring probe).

### Not optional: the anti-throttling flags

A headless page is *occluded by definition*. Without

```
--disable-background-timer-throttling
--disable-backgrounding-occluded-windows
--disable-renderer-backgrounding
--disable-features=CalculateNativeWinOcclusion
```

Chromium throttles background timers to roughly one tick per minute, which
breaks **both** the 45s presence republish and the worker's 60s lease renewal.
The node then ages off the constellation while the process looks perfectly
healthy — the exact silent-failure shape this role exists to avoid.

---

## Supervision: the node, not the process

`Restart=always` only catches "Chromium exited". The failure that matters is
"Chromium is fine, the node is dead" — a wasm panic that takes the worker's
global scope, an admission wedged in a terminal denial, a page that stopped
publishing presence. systemd cannot see any of those.

So the page heartbeats the broker every 15s with its live `RunNodeStatus`; the
broker's `/healthz` is `200` only when that heartbeat is fresh **and** the
worker reports a lifecycle it can actually serve from; and
`hive-browser-node-run.sh` polls it, recycling Chromium after
`4 × 30s = 2 minutes` of continuous unhealth. Consecutive-failure counting is
deliberate — a relay migration or a leader election reports `degraded` briefly,
and one bad sample must never recycle a healthy browser.

---

## Where a browser node says it is (`geo_source`)

The constellation places a browser node by the `lat`/`lon` on its presence
record. There are exactly **two** sources, in priority order, and deliberately
no third:

| `geo_source` | Where it came from |
|---|---|
| `declared` | the host's own `hive_geo` in `inventory/hosts.ini`, the same pin `hive-node` runs with — templated into `HIVE_BROWSER_NODE_LAT/LON` |
| `registry` | the platform's own `is_self` record in `GET /v1/nodes`, i.e. where the fleet already believes this machine is |
| `none` | nothing resolved yet — the record publishes **unplaced**, which is the honest outcome |

There is no geolocation-API call and no browser Geolocation permission: the
latter does not exist headlessly and would be an invention if it did.

The fallback is load-bearing, not decorative. On the live roster only **`va` and
`fr` declare `hive_geo`** — `bkk`, `sj` and `sp` do not (`ansible-inventory
--list`), so without it three of the five fleet browser nodes would publish with
no fix at all, indistinguishable from a node that simply had not published yet.

Resolution never blocks: `/config` answers immediately with whatever is known,
the lookup runs in the background on a 120s retry floor (a failed attempt costs
one mint against the backend's 20-per-60s per-IP limiter, shared with `hive-ui`
on the same loopback address), and the page re-reads `/config` on its own
presence tick while it is still unlocated — so a resolution that only succeeds
after `hive-node` finishes starting is still picked up, without a restart.

```bash
ssh root@<host> 'curl -s localhost:3009/healthz | jq "{geo_source, geo_error}"'
```

To pin a host explicitly, add `hive_geo="lat,lon,city,country"` to its
`inventory/hosts.ini` line — the same var `hive-node` reads.

---

## Verifying a node is live

The role does this itself and **fails the play** if it does not converge, but
by hand:

```bash
# 1. The units
ssh root@<host> 'systemctl status hive-browser-node-broker hive-browser-node --no-pager'

# 2. The node's own view — lifecycle must reach online, admission granted
ssh root@<host> 'curl -s localhost:3009/healthz | jq'
# {"ok":true,"lifecycle":"online","admission":"granted","endpoint_id":"…","serving":true,…}

# 3. The caps are really applied (this is the proof, not the unit file)
ssh root@<host> 'systemctl show hive-browser-node -p MemoryMax -p MemoryHigh -p CPUQuotaPerSecUSec -p TasksMax -p OOMScoreAdjust'
ssh root@<host> 'cat /sys/fs/cgroup/system.slice/hive-browser-node.service/memory.max'

# 4. THE REAL CHECK — the fleet lists it in browser presence
ssh root@<host> 'jar=$(mktemp);
  curl -fsS -c $jar -XPOST localhost:3009/api/token -H "content-type: application/json" -d "{}" >/dev/null;
  curl -fsS -b $jar localhost:3009/cloud/v1/browser/presence | jq ".presence[] | {display_label,state,relay_hint,endpoint_id}";
  rm -f $jar'
```

The `display_label` the platform assigns is `<tenant>-<first 8 hex of endpoint
id>` (`browser_presence.rs`), `state` should be `online`, and the record must
reappear within 90s of every republish. All five nodes also show up together
in one call from any host, since presence is replicated fleet-wide:

```bash
ansible-playbook playbooks/browser-nodes.yml --tags verify
```

The dashboard view is `/network` (the constellation) signed in as the tenant in
`hive_browser_node_team`.

---

## Gotchas

* **Wiping `/var/lib/hive/browser-node` rotates the endpoint id.** The worker's
  persistent ed25519 identity lives in Chromium's IndexedDB inside that
  profile. It is not a cache.
* **Presence and admissions are per-tenant.** Verifying with a token for a
  different team returns an empty list and looks exactly like a dead node.
* **`serving: false` is not a failure.** With `serve_mode: auto` and a tenant
  whose deployments are all servers/containers, the eligible set is legitimately
  empty — the node still joins the mesh, holds its relay identity and publishes
  presence.
* **A `hive-node` restart does not stop the browser node** (`Wants=`, never
  `Requires=`). Stop-propagation would turn every backend rollout into a
  fleet-wide browser-node outage, and a dependency-stop does not re-trigger
  `Restart=`, so the node would stay dark until a human noticed.
* **Chrome pulls a large X/GTK dependency closure** even for headless. That is
  inherent to running the real browser; budget a few hundred MB on first
  install.
