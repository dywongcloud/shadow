# shadw

Command-line interface for the [shadw](https://shadw.cloud) peer-to-peer cloud.

```sh
npx shadw --help
npx shadw auth login
npx shadw deploy
npx shadw projects list
```

Or install it:

```sh
npm install -g shadw
shadw --help
```

## Platforms

This package is a thin dispatcher — `optionalDependencies` pull in the real
binary for your platform automatically:

| Platform | Package | Status |
|---|---|---|
| macOS (Apple Silicon) | `shadw-cli-darwin-arm64` | ✅ published |
| macOS (Intel) | `shadw-cli-darwin-x64` | ✅ published |
| Linux (x64) | `shadw-cli-linux-x64` | 🚧 built by CI on next release |
| Linux (arm64) | `shadw-cli-linux-arm64` | 🚧 built by CI on next release |

If your platform's package isn't published yet, `shadw` will tell you exactly
that (rather than a confusing generic error) and point you here.

## Configuration

```sh
shadw --api <url>      # default http://127.0.0.1:8786 (env: SHADW_API_URL)
shadw --token <key>    # env: SHADW_TOKEN
shadw --team <slug>    # env: SHADW_TEAM
shadw --json           # scriptable output
```

`shadw auth login` saves your API key to `~/.shadw/config.json`.

See also: [`shadw-node`](https://www.npmjs.com/package/shadw-node) — boot a
real local platform node to point this CLI at.

## Source

Built from [`crates/shadw-cli`](https://github.com/dywongcloud/hive/tree/main/crates/shadw-cli)
in this monorepo. `scripts/build-npm-pkg.sh` is the single source of truth for
how every platform package here is built and staged — both local builds and
CI call it.
