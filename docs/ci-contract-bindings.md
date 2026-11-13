# CI contract bindings

`scripts/check-contract-bindings.py --check` is a hard gate in
`.github/workflows/rust-ci.yml` (`contracts` job) and in the npm release
workflow (`verify` job). It is the project's answer to "tests with accurate,
cohesive mocks that have their own update path".

## Why bindings, not mocks

`AGENTS.md` refuses new test files, `#[cfg(test)]` modules and mocking
libraries. A mock is a synthetic stand-in for a construct: it can drift
silently, and a passing test then proves only that the mock still agrees with
itself. A **binding** is the opposite — it pins a live assertion to the
canonical bytes of the real construct that assertion spends. There is no
phantom state. When the construct changes, moves or disappears, CI fails
loudly and names the regeneration command.

The four test layers are RUN, not authored:

| Layer | Blocking command in `rust-ci.yml` |
| --- | --- |
| Unit | `cargo test --workspace --locked --verbose` |
| Integration | `cargo test -p shadw-cli --locked --verbose`, `core-tests` job |
| System | real node + real CLI + persistence restart in `scripts/cli-acceptance.sh` |
| Acceptance | `./scripts/cli-acceptance.sh` end to end |

The `contracts` job asserts every one of those commands is present as a
blocking step, that no undeclared `continue-on-error: true` was added, that
every job has `timeout-minutes`, and that the release workflow gates
`publish` on a test job (`needs: [verify, build]`). A gate removed from CI is
a failed build, not a silently weaker pipeline.

## What is bound

`scripts/contract-bindings.json` holds 33 bindings across the CLI subcommand
enums, both binary crates, `roles::RULES` and its `TEAM_ACTIONS` mirror, the
store-sync `REGISTRY`/`MERGE_STORES` sets, both implementations of the browser
policy digest and `MESH_SIG_DOMAIN`, the two `LANDING_CACHE` copies, the three
`sync-browser-node.mjs` runtime module sets, the acceptance harness's
assertion inventory and its admin-API path set.

Canonical bytes are the source span of ONE construct with comments and
non-semantic whitespace stripped, identifiers and literals kept. The preimage
is LENGTH-PREFIXED (`<len>:<bytes>`) under the domain
`hive-contract-binding-v1\0`, so a bare join cannot collide. SHA-256 is a
BINDING here, not a BLAKE3 content identity: see `AGENTS.md` "Hashing:
content identity vs. security binding".

## Drift classes and the update path

Every failure prints the exact regeneration command. `--sync` is refused in
CI (the checker scans both workflows for it), and the `contracts` job runs
`git diff --exit-code -- scripts/contract-bindings.json` so a regenerated
manifest must ship WITH the construct change in the same commit.

| Class | Cause | Action |
| --- | --- | --- |
| `CHANGED` | construct bytes edited | run `--sync`, review the diff, commit both |
| `MISSING` | file deleted | `--sync --remove ID:reason`, or restore it |
| `MOVED` | symbol renamed / construct migrated | `--sync --move ID:new/path:start-substring` |
| `UNBOUND` | new live assertion with no pinned binding | `--sync` |
| `ORPHAN` | binding still pinned after its construct was retired | `--sync --remove ID:reason` |
| `TRUNCATED` | closing token moved beyond the span limit | re-anchor the binding definition |
| `STALE_PATH` / `STALE_OVERRIDE` / `STALE_REMOVAL` | decision recorded but not synced, or reverted without cleaning up | `--sync` |
| `GATE_CHANGED` | the checker itself was edited without a manifest diff | `--sync` |

`--move` and `--remove` write an `overrides` / `removed` entry plus a
`history` row into the manifest, so a migration or deletion is a recorded,
reviewable decision. Plain `--sync` REFUSES to launder a `MISSING` or `MOVED`
construct: it will not silently drop a binding it can no longer find.

Example, after moving the mesh domain constant into its own module:

```bash
python3 -I scripts/check-contract-bindings.py --check
# CONTRACT_DRIFT integration.browser.mesh.js MOVED ... REGENERATE: ... --sync
python3 -I scripts/check-contract-bindings.py --sync \
  --move 'integration.browser.mesh.js:crates/hive-browser/www/mesh-sig-domain.js:export const MESH_SIG_DOMAIN ='
python3 -I scripts/check-contract-bindings.py --check
cargo test --workspace --locked     # CI's hard gate still runs
```

## Declared fail-open steps

Four steps remain `continue-on-error: true`, each with a reason recorded in
the manifest's `fail_open` array: `rustfmt (check)` and `clippy` (pre-existing
workspace backlog), `cargo audit` and `npm audit` (pinned advisory backlogs).
Anything else non-blocking, or a stale allowance whose step is now blocking,
is a failed build. This is a ratchet: fail-open can only shrink.

The remaining audit steps run with `if: always()` so an install flake cannot
silently skip every later check in the job. `npm ci` no longer falls back to
a floating `npm install`. The committed-secret scan exits nonzero on a PEM
body or an assigned fleet-secret literal instead of printing a warning.

## macOS runners

Every blocking layer runs on macOS too. `rust-ci.yml` uses `macos-15`
(arm64) and `macos-15-intel` (x64) alongside `ubuntu-latest`; the npm release
workflow gates the tagged SHA on the same three runners and builds darwin
packages on the two Mac labels. `macos-13`/`macos-14` were retired on
2026-11-02 and are refused by the contract gate.

`REQUIRED_MACOS_JOBS` in the checker asserts each job's OWN block carries a
macOS runner (a macOS leg in an unrelated job cannot satisfy a missing one),
that both arch labels appear in each workflow, and that no retired label
remains in an executable line. The `cli` job passes `HIVE_CI_PLATFORM` and the
harness refuses a mismatch with `uname -s`, so a mislabeled runner cannot
silently drop the Mac leg.

The real macOS defect was the temp root, not build isolation. `mktemp` on
Darwin ignores `TMPDIR` and uses `/var/folders/...`; measured there the
acceptance harness was **31 passed / 4 failed** (deploy never reached ready).
With the AGENTS.md `/private/tmp` template it is **35 passed / 0 failed** —
the identical assertion set as Linux, no platform branch, zero skips. The
revert-to-default refutation reproduced 31/4, so the fix is causal. The host
build lane runs on macOS even though `build_isolation_protocol` is `null`.
Two bindings (`system.harness.macos.temp`, `system.harness.platform.probe`)
pin these constructs, so changing either without `--sync` fails CI.

Linux CI (run 43151853515) is green at 35/35 acceptance and all four jobs.

## Verified

The Rust lane adds a blocking live witness to the `test` job (all three OSes): an unmodified
Vercel Rust handler builds OFFLINE against the first-party shim and curls `/` and `/_vercel/ping`.
Five bindings pin the shim `$PORT` listener, the clean cargo argv, the platform-owned
`[patch.crates-io]`, the probed `cargo`/`rustc` capability bit and the `Runtime::Command` label.
See `docs/rust-functions.md` for the honest compatibility boundary.

Measured on 2026-11-12: `RESULT PASS bindings=33 layers=4 gates=36
fail_open=4 secrets_clean=1`; `cargo test --workspace --locked` 59 `test
result: ok` lines, exit 0; `cd ui && npx tsc --noEmit -p tsconfig.ci.json` exit 0;
`npm run lint` exit 0; `npm run build` exit 0 (with and without `.env.local`),
`npm ci --dry-run` exit 0, `cargo clippy -p shadw-cli --all-targets --locked
-- -D warnings` exit 0, `git diff --check` exit 0. macOS acceptance
`HIVE_CI_PLATFORM=macos ./scripts/cli-acceptance.sh` = 35 passed / 0 failed,
exit 0; `HIVE_CI_PLATFORM=linux` on macOS exits 1 with `PLATFORM MISMATCH`.
Witnessed drift attacks: `CHANGED`, `MISSING`, `MOVED`, `UNBOUND`, `ORPHAN`,
`GATE_CHANGED`, a drift on the macOS temp-root construct, a full `--move`
migration, a full `--remove` retirement, and a CI run attempting `--sync` —
each refused or detected, tree restored.
