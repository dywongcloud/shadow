# AGENTS.md

Working conventions for this repo, present-tense — what must/must-not be true
of the code now, not a history of how it got that way (see `CHANGELOG.md` for
history).

## Data model

- Relational mirror on GuardianDB for `ProjectStore`/`BillingStore`; hot-path
  `MetricsStore` deliberately excluded; `refresh()`-before-read requirement.
  Detail: `recall("relational-scope")` / `crates/hive-cloud/src/relational.rs`.
- Admin SQL/tables view (`GET/POST /v1/admin/sql/*`) must stay read-only;
  extend `relational::known_tables()` for any new relational table. Detail:
  `recall("sql-readonly-guard")`.

## Mesh networking & anti-entropy

- Per-node embedded relay + `select_relay_hint` selection order; discovery
  fallback (`dial_fresh`) is complementary, not redundant, with the
  anti-entropy loop. New cross-node RPC paths go through `gossip::dispatch`'s
  existing match-arm pattern. Detail: `recall("relay-antientropy")` /
  `crates/hive-cloud/src/main.rs` (`spawn_anti_entropy_loop`), `crates/hive-edge`.

## Secrets

- `ProjectStore::put_env` force-masks credential-shaped values regardless of
  the caller's `sensitive` flag (`project_settings::looks_like_secret`) —
  extend the prefix list for new providers rather than trusting the UI
  checkbox alone. Detail: `recall("secret-detection")`.

## Process

- Git only via the `gm` skill's git verbs (`git_finalize`, `git_push`, etc.)
  — never raw `git` via a shell, which bypasses the porcelain-clean gate.
- No test files, ever — no `test/`/`__tests__/`/`spec/` directories, no new
  `#[cfg(test)]` modules for verifying a change. Verify behavior via the
  existing test suite (`cargo test --workspace`) plus real, live execution
  (curl against a running node, SSH to a fleet node) — never a mock standing
  in for a real service.
- Fleet has two glibc groups needing separate native builds (bkk/hk vs
  va/va2/va3/sj); always sha256-verify + `.old`-backup a binary before
  swapping. Detail: `recall("fleet-glibc-groups")`.
- The dashboard's `/ops/*` proxy forwards every admin request to the CURRENT
  control-plane leader, not the node running the dashboard process — verify
  new admin endpoints through the real dashboard. Detail:
  `recall("ops-proxy-leader-forward")`.
