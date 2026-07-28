# shadw — platform CLI

The official command-line interface for the **shadw** peer-to-peer cloud. It wraps
the platform REST API and authenticates with the **API key** you generate in the
dashboard (**Settings → API Keys**). Every request is scoped server-side to that
key's team.

## Install / build

```bash
cargo build -p shadw-cli          # produces target/debug/shadw
cargo install --path crates/shadw-cli   # installs `shadw` on your PATH
```

## Authenticate

Create a key in the dashboard (Settings → API Keys), then:

```bash
shadw auth login --token hive_xxxxxxxx --api https://api.shadw.cloud
shadw auth whoami
```

**Note (enforced platforms):** the ingress accepts only platform JWTs on
mutations, so an API key alone is read-only there. For full read+write
auto-login (self-hosted operators holding `HIVE_INTERNAL_TOKEN`), use
**mint-mode** — the CLI then mints and auto-refreshes a 1-hour platform JWT
transparently, caching it in the config to stay under the mint rate limit:

```bash
shadw auth login --api https://api.shadw.cloud \
  --email you@example.com --internal-token "$HIVE_INTERNAL_TOKEN"
```

`login` saves `~/.shadw/config.json`. Config is resolved (highest priority first)
from **flags → environment → config file → defaults**:

| Setting | Flag      | Env vars                          | Default                  |
|---------|-----------|-----------------------------------|--------------------------|
| API URL | `--api`   | `SHADW_API_URL`, `SHADW_API`      | `http://127.0.0.1:8786`  |
| Token   | `--token` | `SHADW_TOKEN`, `SHADW_API_KEY`    | —                        |
| Team    | `--team`  | `SHADW_TEAM`                      | (inferred from the key)  |

Add `--json` to any command for raw, scriptable JSON (pipe into `jq`).

## Commands

```text
shadw deploy <repo_url> [--branch B] [--project P] [--root-dir D]
            [--target production|preview] [--no-cache] [-e KEY=VALUE]... [--follow]
shadw build (get|logs) <build_id> [--follow]

shadw deployments|deps   list | promote <id> | delete <id> | resources <id>
shadw projects|proj      list | settings <p> | redeploy <p> [--target][--no-cache]
                         | delete <p> | domain <p> <domain>
                         | env (list <p> | set <p> <K> <V> [--target][--sensitive] | rm <p> <K>)
shadw domains|dom        list | get <d> | scan <d> | import <d> --zone FILE
                         | nameservers <d> <ns...> | ssl-renew <d>
                         | record (list <d> | add <d> --type --name --value [--ttl][--priority]
                                  | update <d> <id> --type --name --value [--ttl] | rm <d> <id>)
shadw webhooks|hooks     list | events | deliveries | create <url> [--project][--events] | delete <id>
shadw teams              list | get <slug> | member (add|rm) <slug> <email> [--role] | plan <slug> <plan>
shadw keys|apikeys       list | create <name> [--role] | revoke <id>
shadw databases|db       list | get <id> | create <name> [--engine] | delete <id> | credentials <id>
shadw cron               list | add <name> <schedule> <url> | delete <id>
shadw workflows|wf       list | runs | run <id>
shadw obs                overview | logs [--limit] | resources | metrics | regions | nodes
shadw auth               login | whoami | token | logout

shadw api <METHOD> <path> [--data JSON | --data-file FILE]   # generic passthrough — any endpoint
```

## Examples

```bash
# Deploy a repo to production and tail the build logs
shadw deploy https://github.com/acme/app --branch main --target production --follow

# Roll back instantly by promoting a prior deployment
shadw deps list
shadw deps promote dpl-46f4fb3397

# Manage env vars + DNS
shadw proj env set acme-app DATABASE_URL "postgres://…" --sensitive
shadw dom record add acme.com --type A --name @ --value 76.76.21.21

# Anything not covered by a typed command:
shadw api GET /v1/overview | jq .deployments
```

## Tests

The CLI and its dependency chain are covered at four levels (real components
preferred; mocks only where they're a deterministic 1:1 shim):

| Level | What | Where |
|-------|------|-------|
| **Unit** | config precedence (flag→env→file→default), URL join, deploy-body builder, env parsing, token masking, output rendering | `src/{client,main,output}.rs` `#[cfg(test)]` |
| **Unit (deps)** | the stores the CLI drives: API keys (create/verify/revoke/team-scope, hash never leaked), project env (encrypt-at-rest/mask/decrypt), DNS records (CRUD, system-record immutability, idempotent import), secrets (seal/open round-trip) | `crates/hive-cloud/src/{apikeys,dns,project_settings,secrets}.rs` |
| **Integration** | the real `shadw` binary against a real local HTTP server (records the wire contract: bearer auth, team header, method/path/JSON body, error handling) | `tests/http_contract.rs` |
| **Acceptance / system** | the real CLI ↔ a real hive-cloud node ↔ in-memory stores ↔ on-disk persistence, full lifecycle incl. **state survival across a node restart** | `scripts/cli-acceptance.sh` |

```bash
cargo test -p shadw-cli            # unit + integration
cargo test -p hive-cloud           # dependency-store unit tests
./scripts/cli-acceptance.sh        # end-to-end acceptance (spawns a throwaway node)
```
