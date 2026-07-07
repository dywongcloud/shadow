# RUNBOOK — ngrok → real DNS (`shadw.cloud` + `*.shadw.app` via Vercel DNS)

The platform serves everything on its own domains: `api.shadw.cloud` (control
plane), `*.shadw.app` (every deployment alias, one label), with TLS terminated
by the nodes themselves (ACME DNS-01 wildcard via the Vercel API) and DNS
records reconciled from live node health by a leader-elected loop.

Two registrable domains is deliberate (the vercel.com / vercel.app split): user
content on `shadw.app` can never set cookies on, or shadow, `shadw.cloud`.
Never serve user apps from a `shadw.cloud` subdomain.

---

## Phase 0 — Preflight (HUMAN steps; nothing works without these)

1. ~~Add both domains to the Vercel account~~ **DONE (2026-07-03)** — the
   current token sees `shadw.app` + `shadw.cloud`, both verified.
2. ~~Registrar nameservers → vercel-dns~~ **DONE** — both domains resolve via
   `ns1/ns2.vercel-dns.com`. CAA already allows `letsencrypt.org` on both.
   **PROVEN LIVE**: an LE **staging** wildcard cert for `*.shadw.app` +
   `shadw.app` was issued end-to-end through the Vercel DNS-01 solver
   (TXT write → propagation → validation → chain → cleanup, ~20s).
   NOTE: both zones carry Vercel's default `ALIAS` records on `*`/apex —
   the reconciler DISPLACES them automatically (only when publishing real
   addresses for those names).
3. **Vercel API token** with DNS-records scope. If the domains live in a team,
   note the team and set `VERCEL_TEAM_ID`. Store the token as a SECRET
   (systemd drop-in / launchd env; never in the repo, never logged).
4. **Every production node** must set `HIVE_PUBLIC_IP` (and `HIVE_PUBLIC_IP6`
   where available) — nodes without it are never published to DNS.
5. **Firewall**: open 80/tcp + 443/tcp on every published node (note:
   `hive-lockdown.sh` currently blocks public 8787/9090 — 80/443 must be
   ALLOWED).
6. **Low ports without root**: systemd drop-in
   ```ini
   # /etc/systemd/system/hive-node.service.d/ingress.conf
   [Service]
   AmbientCapabilities=CAP_NET_BIND_SERVICE
   Environment=HIVE_INGRESS=dual
   Environment=HIVE_APPS_DOMAIN=shadw.app
   Environment=HIVE_PLATFORM_DOMAIN=shadw.cloud
   Environment=VERCEL_API_TOKEN=<secret>
   Environment=HIVE_ACME_EMAIL=<ops email>
   Environment=HIVE_ACME_STAGING=1
   ```
   (or `setcap 'cap_net_bind_service=+ep' /root/fc-target/release/hive-cloud`
   after every binary swap — the systemd capability survives swaps, setcap does
   not; prefer the drop-in.)
7. **`HIVE_JWT_SECRET` must be set on every node** before any non-ngrok ingress
   mode: `api.shadw.cloud` exposes the admin router publicly and the node
   refuses to host-split without JWT enforcement.
8. Relay/discovery names: set `HIVE_RELAY_IPS` / `HIVE_DISCOVERY_IPS`
   (comma-separated public IPs of the nodes running the self-hosted iroh relay
   / pkarr relay) on the LEADER-capable nodes so
   `relay.shadw.cloud`/`discovery.shadw.cloud` publish.

## Configuration reference

| Var | Meaning | Default |
|---|---|---|
| `HIVE_PLATFORM_DOMAIN` | platform domain | `shadw.cloud` |
| `HIVE_APPS_DOMAIN` | wildcard apps domain | `shadw.app` |
| `HIVE_INGRESS` | `ngrok` \| `dual` \| `dns` | `ngrok` |
| `VERCEL_API_TOKEN` | Vercel REST token (secret) | — |
| `VERCEL_TEAM_ID` | optional teamId | unset |
| `HIVE_ACME_EMAIL` | ACME account email | `ops@shadw.cloud` |
| `HIVE_ACME_STAGING` | `1` = LE staging (KEEP during all dev) | `1` |
| `HIVE_HTTP_LISTEN` / `HIVE_HTTPS_LISTEN` | public listeners | `0.0.0.0:80` / `0.0.0.0:443` |
| `HIVE_DNS_RECONCILE_SECS` | reconcile cadence | `30` |
| `HIVE_DNS_RECONCILE=1` | force reconciler in ngrok mode (testing) | off |
| `HIVE_ACME_FORCE=1` | force ACME in ngrok mode (testing) | off |
| `HIVE_ACME_PLATFORM_EXTRA=1` | add relay./discovery. to the api cert | off |
| `NEXT_PUBLIC_INGRESS` (UI build) | mirrors HIVE_INGRESS | `ngrok` |
| `NEXT_PUBLIC_DEPLOYMENT_DOMAIN` (UI build) | `shadw.app` at cutover | current ngrok zone |

## Cutover sequence

1. **Bake code fleet-wide** with `HIVE_INGRESS=ngrok` (default): zero behavior
   change (regression-tested — everything new is flag-gated).
2. Set the Phase-0 env on all nodes → `HIVE_INGRESS=dual`, restart. Watch:
   - `/v1/relay` → `dns_reconciler` counters (creates on first pass; then
     no-op passes), `tls_zones` (populates after the leader's first ACME run
     against **staging**).
   - `dig api.shadw.cloud +short` / `dig anything.shadw.app +short` → healthy
     node IPs only, TTL 60.
3. Staging TLS proven end-to-end → set `HIVE_ACME_STAGING=0` on the leader
   candidates, delete the staging bundles (`$HIVE_DATA/tls-*-staging.json`),
   restart → production certs. (LE prod limit: 5 duplicate certs/week — do not
   iterate against prod.)
4. Rebuild the dashboard with `NEXT_PUBLIC_INGRESS=dual` +
   `NEXT_PUBLIC_DEPLOYMENT_DOMAIN=shadw.app` → deployment links move to
   `https://<alias>.shadw.app`.
5. Bake days on `dual` (both ingress paths serve identical content).
6. `HIVE_INGRESS=dns` fleet-wide + stop/disable the ngrok systemd/launchd
   units. URL generation is fully on the new domains; ngrok host suffixes are
   dropped from `host_allowed`.

## Break-glass (back to ngrok)

1. Set `HIVE_INGRESS=dual` (or `ngrok`) on all nodes; restart.
2. Re-enable/start the ngrok units (`systemctl start ngrok…` / launchd on the
   Macs). The ngrok code path is intact behind the flag — nothing was deleted.
3. Rebuild the dashboard with `NEXT_PUBLIC_INGRESS=ngrok`.

## Failover expectations

Node death → active prober flips health (~10–12s) → damping (1 extra reconcile
pass) → reconcile (≤30s) → record TTL (60s): a dead node's IP is out of
circulation in **< 2 minutes**; surviving IPs keep serving throughout. The
reconciler can NEVER publish an empty record set (keeps last-known-good and
raises an incident instead).

## Notes

- The self-hosted Seer DNS (`dnsserver.rs`) still runs for internal/test
  resolution and the future NS-delegation phase — not public in this phase.
- Regional/latency steering happens inside `edge.rs` after connect; DNS only
  hands out healthy IPs.
- The dashboard itself (shadow.ngrok.pizza) and apex `shadw.cloud` marketing
  site are OUT of this migration's scope.
- Custom customer domains and per-deployment certs are follow-ups.

---

# Adding a node to the mesh (hot-join, key-addressed)

Peers dial each other by iroh **key** (a 64-hex NodeId), never by IP — iroh's
QUIC transport resolves the address via discovery/relay/holepunch underneath.
A public IP is only ever needed for the client-facing DNS ingress (Plane A),
never for mesh membership.

## Adding a node — the whole procedure

On the NEW node, set env vars and start `hive-cloud` — no `--peer` HTTP URL,
no existing node touched, no restart of anything already running:

```
HIVE_JWT_SECRET=<the fleet's shared secret>
HIVE_BOOTSTRAP_PEERS=<any one existing node's 64-hex iroh NodeId>
HIVE_DISCOVERY_DNS=<the fleet's self-hosted discovery URLs>
```

`HIVE_DISCOVERY_DNS` is REQUIRED on this fleet (not merely nice-to-have):
every existing node runs `HIVE_DISCOVERY_N0=0`, opting OUT of iroh's public
n0 pkarr discovery in favor of the self-hosted relay — a joiner that skips
this env var can resolve n0-discoverable peers but never a fleet node, and
the bare-key seed silently never connects. Copy the value from any running
node's environment (`HIVE_DISCOVERY_DNS=http://<node-ip>:3350,...` — one
entry per discovery-capable node). Verified live: a joiner WITHOUT this var
sat inert (registered the seed, dialed nothing observable); adding it joined
the fleet in under 5 seconds.

The new node then:

1. Dials the seed key (`HIVE_BOOTSTRAP_PEERS`) over iroh — discovery/relay/
   holepunch resolve it, IP-free.
2. Presents a join proof (`HMAC-SHA256(HIVE_JWT_SECRET, its own iroh NodeId)`)
   over a dedicated join stream. The proof is checked against the QUIC
   connection's cryptographically authenticated remote identity — never a
   value the request body claims — so only a genuine holder of the fleet
   secret can self-admit, and only for its own key.
3. On admission the seed returns its live node list; the new node appears in
   every node's `/v1/nodes` within one gossip round (~5s) as each node's
   gossip loop dials the newly learned key directly (dynamic per-round
   targets, not a fixed list computed once at boot).
4. The roster (the key-addressed dial map, never IPs) is persisted to
   `$HIVE_DATA/peer_iroh.json` and replicated into GuardianDB (iroh-docs),
   so a wiped node re-adopts the mesh from the replicated copy if its local
   file is lost.

Leaving the mesh: stop the process. The node ages out of every peer's
registry after ~30s of missed gossip; no cleanup step required.

## Operator-side admission (optional, explicit allowlist)

If a fleet runs with `HIVE_PEER_TRUST=1` (opt-in strict allowlist) and you'd
rather pre-authorize a key out-of-band instead of relying on the join-proof
self-admit:

```
POST /v1/mesh/admit  { "endpoint_id": "<64-hex NodeId>" }   (operator token)
GET  /v1/mesh/roster                                          (operator token)
```

`admit` inserts the key into this node's trust set immediately (audited);
it becomes visible to other nodes transitively the next time this node
gossips a NodeInfo carrying that key's iroh address.

## What does NOT change on a hot-join or on a restart

- **DNS** (Plane A, `dnsserver.rs`/Vercel reconciler): entirely separate from
  gossip; unaffected by any peer joining, leaving, or by a `hive-cloud`
  restart on another node.
- **The dashboard** (`ui/`, port 3002): its own OS process/launchd unit;
  restarting or redeploying it never touches gossip/mesh/DNS state.
- **Existing peers**: a hot-join needs zero restarts anywhere in the fleet.
  A `hive-cloud` restart on ONE node re-joins the mesh from its own
  persisted roster/GuardianDB replica + iroh's persistent identity —
  it does not require reconfiguring or restarting any other node.
