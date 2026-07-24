# AGENTS.md

Working conventions for this repo, present-tense — what must/must-not be true
of the code now, not a history of how it got that way (see `CHANGELOG.md` for
history).

## Data model

- Relational mirror on GuardianDB: 11 tables (projects/billing + view-only
  teams/members/deployments via `spawn_relational_mirror_loop`); billing is
  normalized real per-row tables (`billing_accounts`, `billing_ledger`,
  `billing_invoices` + `billing_invoice_lines`, `billing_checkouts`), not
  JSON blobs — `billing_ledger_snapshot`/`billing_invoices_snapshot` remain
  in the DDL deprecated-pending-removal (no longer written); hot-path
  `MetricsStore` deliberately excluded; `refresh()`-before-read requirement.
  `relational::upsert_billing` wraps a tenant's whole write (account + every
  ledger/invoice/invoice-line/checkout row) in one `BEGIN; ...; COMMIT;`
  transaction — never split back into separate autocommit statements.
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

## Deploys

- `git push` auto-deploys through TWO independent triggers, never assume the
  webhook is the only one. (1) The GitHub webhook (`admin::git_webhook`,
  `/v1/git/webhook`) fires only when a hook was actually installed — a project
  imported as a plain public repo URL, or whose owner never completed the
  GitHub connection, has `git_ci == None` and NO hook, so GitHub never notifies
  the platform. (2) `git::spawn_git_poll_reconcile` (leader-only, registered in
  `main.rs`) covers exactly that gap: it polls every git-sourced project's
  tracked-branch HEAD with `git ls-remote` and starts the SAME build the webhook
  would whenever HEAD has advanced past the deployed commit. Both dedup on the
  commit SHA (`CloudState::git_poll_seen`, seeded from the deployed commit) so a
  push deploys exactly once regardless of which trigger sees it first — never
  add a deploy path that bypasses that SHA check. A public repo needs no
  credential; a private repo reuses `git_webhook`'s token resolution
  (`github_app_auth` install token, else node `GITHUB_TOKEN`).

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
- `scripts/shadw-watchdog.sh` is a PERSISTENT KeepAlive loop (`WATCHDOG_LOOP=1`,
  never StartInterval); needs one manual `launchctl kickstart` after any plist
  re-bootstrap, and its `ensure()` targets track the current `dev.shadw.fc-lax*`
  labels. Detail: `recall("launchd-pended-spawn-gotcha")` /
  `recall("fc-lax2-watchdog-incident")`.
- The backend (`hive-cloud`, systemd `hive-node`) and the dashboard (`ui/`,
  systemd `hive-ui`) are deployed independently — a backend-only fleet
  rollout does NOT ship a `ui/` change. Use `scripts/deploy-ui-fleet.sh`
  for any `ui/`-touching change. Detail: `recall("ui-deploy-gap-incident")`.

@.gm/next-step.md
