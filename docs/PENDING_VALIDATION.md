# PENDING_VALIDATION

Each line names a change, its PRD row, and the witness that must run before a push.
Phase A writes the changes and commits them locally; Phase B runs these witnesses
once, fixes only what fails, reruns only what failed, then pushes.

| file | row | witness |
| --- | --- | --- |
| `CHANGELOG.md` | `acme-san-gated-on-single-public-resolver` | standalone `rustc` compile of the dependency-free `crates/hive-cloud/src/dns_quorum.rs` plus a probe exercising `Tally::admits` (split 2/1/2 admits, one operator does not, 2/2 tie drops), and a literal scan of the `public_answers` -> `operator_votes` -> `tally` -> `admits` chain in `acme::effective_sans`. Contract-change reason recorded in the changelog entry. |
| `CHANGELOG.md` | `aaa-read-window-error-swallow-2026-11-16` | shipped `contract-persist` binary builds a real store; its cold `reopen` phase must refuse a segment with a corrupt line (`Corrupt`, nonzero exit) and boot cleanly after that segment is deleted (`CHILD_INDEX_HASH` rebuilt). Plus `cargo test --workspace --locked` RUN and `sh scripts/cli-acceptance.sh`. No change to the fail path. |
| `CHANGELOG.md` | `aaa-upsert-env-idempotence-2026-11-16` | literal scan of `project_settings.rs` proving `touch<'a>` stamps a strictly monotonic `updated_ms`, `merge_take` compares it, `merge_synced` is newest-per-row and never wholesale, plus `cargo test --workspace --locked` RUN. Decision recorded, no change. |
| `AGENTS.md`, `ui/AGENTS.md`, `ansible/AGENTS.md` | `agents-md-size-cap-blocks-prose-addition` | HELD. `wc -c` measured this turn: 30717 / 13996 / 11998 bytes; no enforcing hook was found, so the "~30 KB" line is an editorial guideline, not a cap. Needs a full recompile-from-code audit before any rule prose is added. |
| `crates/hive-cloud/src/admin.rs`, `crates/hive-cloud/src/gossip.rs` | `api-key-create-invisible-node-local-read` | HELD. Needs a two-node local witness (just-created key visible on the FIRST follower GET after POST) and an operator-authorised roll; a push auto-deploys through two triggers. |
| `crates/hive-cloud/src/main.rs` | `aaa-request-path-panic-hardening-2026-11-16` | HELD. Two refutations: a poison-injection witness is missing and `leader_client` still returns `Option<Client>` rather than a typed error. Re-scope with `callers`/`impact` on `leader_client` before any edit. |
| `crates/hive-cloud/src/admin.rs` | `admin-read-route-classification` | HELD. Needs a 7-day report mode, a UI-first deploy and a live HTTP re-witness; production change requiring the operator's word. |

## Phase B results (run this turn)

- `cargo test --workspace --locked` RUN: `CARGO_TEST_EXIT=0`, 61 `test result:` lines, 0 `FAILED`.
- `sh scripts/cli-acceptance.sh`: `CLI_ACCEPTANCE_EXIT=0`, `35 passed, 0 failed`.
- `/usr/bin/python3 -I scripts/check-contract-bindings.py --check`: `RESULT PASS bindings=34 layers=4 gates=35 fail_open=4 secrets_clean=1`.
- `cargo run -p hive-contract --locked --offline --bin contract-persist`: `RESULT: ALL PROBES PASSED`, exit 0.
- `cargo test -p hive-contract --locked --offline`: exit 0.

No push or roll was performed: a `git push` auto-deploys through the webhook and
`git::spawn_git_poll_reconcile` triggers, which needs the operator's word.
