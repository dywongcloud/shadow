# shadw-node

Boot a real [shadw](https://shadw.cloud) platform node locally — as easily as
any other `npx` dev tool. This runs the actual `hive-cloud` server binary (the
same one every fleet node runs), not a simulation of one.

```sh
npx shadw-node
```

That's it. It boots a single local node with sane dev defaults:

```
HIVE_FORCE_MOCK=1 HIVE_DATA=~/.shadw-node hive-cloud \
  --name local --listen 127.0.0.1:8787 --admin 127.0.0.1:8786
```

Then:

```sh
curl http://127.0.0.1:8786/healthz
npx shadw --api http://127.0.0.1:8786 auth login   # point the CLI at it
```

Pass any argument yourself and you get full control, zero injected defaults —
`shadw-node` becomes a thin exec wrapper around `hive-cloud` at that point:

```sh
npx shadw-node --name mynode --listen 0.0.0.0:8787 --admin 127.0.0.1:8786
```

## Why `HIVE_FORCE_MOCK=1` by default

This tool is for **local dev and demos**, not production fleet nodes.
Production nodes are provisioned through this repo's own ansible roles, which
do real hardware/KVM detection, mesh bootstrap, TLS certificates, and more.
`shadw-node` mirrors `scripts/dev-cluster.sh`'s single-node shape instead:
functions run as real host processes (the same isolation tier the platform's
own mock-backend fleet nodes use), with no assumption that your machine has
KVM or Firecracker available. Set `HIVE_FORCE_MOCK=0` yourself if you know
what you're doing and want the real backend-selection logic to run.

## Platforms

| Platform | Package | Status |
|---|---|---|
| macOS (Apple Silicon) | `shadw-node-darwin-arm64` | ✅ built + locally verified (real node booted, `healthz` answered) |
| macOS (Intel) | `shadw-node-darwin-x64` | 🚧 builds via `scripts/build-npm-pkg.sh` or CI (not yet published) |
| Linux (x64) | `shadw-node-linux-x64` | 🚧 built by CI (`.github/workflows/release-npm-cli.yml`) on the next `cli-v*` tag |
| Linux (arm64) | `shadw-node-linux-arm64` | 🚧 built by CI on the next `cli-v*` tag |

This is a real platform server binary (~75 MiB, stripped) — large compared to
a typical `npx` tool, and that's inherent to shipping a real distributed
systems node rather than a bug to fix.

## Source

Wraps [`hive-cloud`](https://github.com/dywongcloud/hive/tree/main/crates/hive-cloud)
in this monorepo. `scripts/build-npm-pkg.sh` is the single source of truth for
how every platform package here is built and staged.
