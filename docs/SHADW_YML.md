# shadw.yml and the Shadw GitHub Action

`shadw.yml` is the standard, declarative Shadw project file. It sits at the
root of an application repository and is read in two places:

- the **Deploy to Shadw** GitHub Action ([`action.yml`](../action.yml)), and
- the **Shadw build itself** ([`crates/fluid-build/src/shadw_config.rs`](../crates/fluid-build/src/shadw_config.rs),
  applied in `hive-cloud` `run_build`), so deploys from Shadw's own GitHub
  integration, redeploys and webhooks follow the same file.

Precedence everywhere: **explicit deploy request > `shadw.yml` > persisted project
settings**. At build time the platform applies `root_dir` and `build.cache`
(the log shows `shadw.yml v1 loaded …` and `Root directory` source
`shadw_yml`). `env` values are applied only by the Action — the build never
persists values from a repository file and logs just the keys. `target`,
`branch` and `deploy.*` are request-time settings used by the Action. Like
`vercel.json`, the file must be a regular (non-symlink) file under 64 KiB;
an invalid file fails the build instead of being ignored, and having both
`shadw.yml` and `shadw.yaml` is an error.

## Quick start

1. In Shadw, create an API key (Shadw → API keys, `shadw_…`).
2. In the app repository, add it as the secret `SHADW_TOKEN`.
3. Add [`shadw.yml`](../examples/shadw.yml) and
   [`.github/workflows/shadw.yml`](../examples/github-workflow-shadw.yml).

```yaml
- uses: actions/checkout@v4
- uses: dywongcloud/shadow@main
  with:
    token: ${{ secrets.SHADW_TOKEN }}
```

The repository must be clonable by Shadw (public, or connected through the
Shadw GitHub integration): the action asks Shadw to build the exact commit,
it does not upload the workspace.

## Format (v1)

| Key | Type | Maps to `POST /v1/git/deploy` |
| --- | --- | --- |
| `version` | `1` (required) | — |
| `project` | DNS label | `project` |
| `root_dir` | relative path | `root_dir` |
| `branch` | string | `branch` (default: triggering branch; when set, the branch tip is built instead of the triggering commit) |
| `target` | `auto` \| `production` \| `preview` | `target` (`auto` omits it; pull requests become `preview`) |
| `build.cache` | bool | `use_cache` |
| `env` | map of scalars | `env` (runtime-only; merged with the action `env` input, input wins) |
| `deploy.wait` | bool | follow `GET /v1/builds/:id` until `ready`/`error` |
| `deploy.timeout_minutes` | 1–120 | wait budget |

Unknown keys are rejected. JSON Schema: [`schemas/shadw.schema.json`](../schemas/shadw.schema.json)
(add `# yaml-language-server: $schema=<raw url>` for editor validation).
Secrets never belong in `shadw.yml`; pass them through the action `env` input
from GitHub secrets — values are masked in logs and the printed request.

## Action inputs / outputs

Inputs: `token` (required), `api-url` (required — your Shadw API base, same as
`SHADW_API_URL` for the `shadw` CLI), `team`, `config`, `project`,
`target`, `env`, `wait`, `dry-run`. Outputs: `build-id`, `project`, `state`, `url`.

## Making it appear in GitHub

- **Marketplace / "uses:"** — the root `action.yml` (with `branding`) is what
  GitHub recognises as an action. Publishing to the Marketplace is a manual
  step on a GitHub release (tick "Publish this Action to the GitHub Marketplace").
  Tag releases (`v1`) so users can pin `@v1` instead of `@main`.
- **Actions tab "New workflow" suggestion** — GitHub only reads starter
  workflows from an organisation's `.github` repository. Copy
  [`workflow-templates/`](../workflow-templates) into
  `ThothDivision/.github/workflow-templates/` to have "Deploy to Shadw"
  offered to every repo in the org (suggested when a repo contains
  `shadw.yml`, `package.json` or `vercel.json`).

## Test

```bash
python3 scripts/shadw-action/test_deploy.py -v
SHADW_REPO_URL=https://github.com/acme/app SHADW_CONFIG=examples/shadw.yml \\
  SHADW_DRY_RUN=true python3 scripts/shadw-action/deploy.py
```
