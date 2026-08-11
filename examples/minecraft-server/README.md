# Minecraft server on shadw

A real, working [`itzg/minecraft-server`](https://github.com/itzg/docker-minecraft-server)
deployment — Minecraft Java Edition, raw TCP, with a persistent world.

## Deploy

```sh
shadw deploy https://github.com/<you>/<this-repo-or-a-copy>.git --project my-minecraft
```

Or import the repo through the dashboard's "New Project" flow. The platform
detects `compose.yaml` automatically — no Dockerfile needed for this example.

Once the build finishes, check the deployment record for the public port the
platform allocated:

```sh
shadw deployments list --json | jq '.[] | select(.project=="my-minecraft") | .raw_ports'
```

You'll get something like:

```json
[{ "container_port": 25565, "function": "mc", "protocol": "tcp", "public_port": 20123 }]
```

Connect a real Minecraft client to `<your-project>.shadw.app:20123` (the
`public_port`, **not** 25565 — 25565 is only the port *inside* the
container). The hostname is round-robin across the fleet; the port is the
actual routing key for raw TCP/UDP services (see "How this works" below).

## Why the `/tcp` suffix in `ports:` matters

```yaml
ports:
  - "25565:25565/tcp"   # correct — raw TCP, gets a public port
  - "25565:25565"       # WRONG for Minecraft — silently treated as HTTP, gets none
```

Docker Compose's `ports:` syntax has no native way to say "this is a non-HTTP
TCP service" — Compose itself doesn't distinguish; both forms above are
equally valid *Docker* syntax. This platform reads the explicit `/tcp`
suffix as that signal. Omit it (or write a bare `"25565:25565"`, which is
what a Minecraft server compose file looks like on every other host) and the
service is classified as an ordinary web service — no public raw port is
ever allocated, and the deployment builds successfully but is simply
unreachable on 25565 from anywhere. This is exactly the bug that broke the
first two Minecraft deployments on this platform; if you copy a `compose.yaml`
from elsewhere for a non-HTTP TCP service, add `/tcp` explicitly.

For a **UDP** service, the equivalent suffix is `/udp` (e.g. Minecraft
Bedrock Edition's `19132:19132/udp` — a completely different protocol from
Java Edition, don't mix them up).

## Exposing a second port (e.g. RCON)

Only the **primary** service (the one the platform decides is your public
entrypoint — a conventional name like `web`/`app`/`mc`, or the first
published service) gets an automatic raw-port allocation. A second,
non-primary service that also wants to be reachable from outside opts in
explicitly with `x-shadw-expose`:

```yaml
services:
  mc:
    image: itzg/minecraft-server:latest
    ports: ["25565:25565/tcp"]
    environment: { EULA: "TRUE" }
  rcon:
    image: itzg/minecraft-server:latest # same image, RCON is built in
    ports: ["25575:25575/tcp"]
    x-shadw-expose:
      protocol: tcp
```

Without `x-shadw-expose`, a non-primary service stays reachable only from
its siblings on the shared compose network (by service name) — never from
the public internet. That's the correct default for a database or cache
sidecar, and it's why RCON needs the explicit opt-in above.

## Persistence

The `./data:/data` volume maps to a real, durable, host-backed named volume
(`hive-vol-<project>-mc`) that survives container restarts *and* redeploys —
your world is not regenerated every time you push. There's nothing else to
configure; every container-runtime deployment gets this automatically.

## Verifying it's actually working

A bare TCP connect (`nc`, `curl`, a browser) isn't a real test — Minecraft's
server list ping requires sending an actual handshake packet before the
server responds with anything. If you want to check without launching the
game client:

```sh
python3 -c "
import socket, struct
def v(n):
    o=b''
    while True:
        b=n&0x7F; n>>=7
        o+=bytes([b|0x80]) if n else bytes([b])
        if not n: break
    return o
def s_(x): b=x.encode(); return v(len(b))+b
host, port = 'my-minecraft.shadw.app', 20123  # your public_port from above
p = v(0)+v(765)+s_(host)+struct.pack('>H',port)+v(1)
pk = v(len(p))+p
sr = v(0); spk = v(len(sr))+sr
sock = socket.create_connection((host, port), timeout=8)
sock.sendall(pk); sock.sendall(spk)
print(sock.recv(4096))
"
```

A real server answers with a JSON status blob (`{"description":...,
"players":{...},"version":{...}}`). No response, or an immediate connection
close with no bytes at all, means something is still misconfigured — check
`shadw build get <build-id>` for the exact log line ("Allocated public raw
port(s): ..." should appear) and confirm the deployment's `raw_ports` field
is populated as shown above.
