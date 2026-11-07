# Documentation index

Present-tense and design documents for the hive tree. A `Status:` line near the top of each
design doc says whether its code exists; history lives in `CHANGELOG.md` and in `.gm/prd/`.

## Smart contracts and the verifiable database

Read in this order:

1. `docs/smart-contracts.md` - what "smart contract" means here (contract-as-DATA, no VM), the
   declared `hive.managed-db.v1` op table, `Engine::submit`/`Executor::commit`, where the layer is
   wired, and the claims that were corrected against the tree.
2. `docs/verifiable-database.md` - the append-only hash-chained oplog: length-prefixed canonical
   bytes, BLAKE3 domains, ack binding, inclusion and append-only-violation proofs, adjudication,
   checkpoint windows, replay-based replication, and the trust model (no consensus, no bond, no
   finality). Includes the Vitra whitepaper section mapping.
3. `docs/contract-platform-seam.md` - how `crates/hive-cloud/src/db_contract.rs` records writes:
   mutate FIRST / record AFTER, measurement outcomes, unattributed REFUSED writes, and where real
   authorization lives. Read this before claiming the contract prevents anything.
4. `docs/contract-verification.md` - the eight `contract-*` binaries, exact arguments, the
   `$HIVE_DATA/contract-oplog` on-disk format, environment knobs and the runbook. Note that
   `--state` is accepted by only four of eight binaries and is optional in all four.

Do not confuse with `docs/browser-db-contract.md`, which is a written SPECIFICATION for the
browser-replicated cr-sqlite database (ownership, ACL, caps, retention, naming); it is not an
oplog or execution-transparency contract.

## Architecture and audits

- `docs/security-architecture-audit-2026-09-22.md` - security architecture audit vs. the proposed
  layered security model; verified live, not a certification document.
- `docs/COMPLIANCE_AUDIT.md` - compliance/security audit (SOC1/SOC2/ISO27001/GDPR/HIPAA + CSP).
- `docs/leaderless-migration.md` - control-plane migration design (DESIGN ONLY; its use of "consensus"
  is about Raft/openraft, unrelated to the contract oplog).
- `docs/pqc-migration-scope.md` - post-quantum migration scope RFC.
- `docs/per-route-execution.md` - Next.js per-route function splitting.
- `docs/BUN_RUNTIME.md` - Bun as a deployment/runtime target.
- `docs/lagon.md` - upstream Lagon runtime overview.
- `docs/pvm-upstream-report.md` - PVM 6.12 to 7.1 port bug report.

## Account security

- `docs/account-2fa.md` - account two-factor authentication (2FA).

## Fleet operations and incidents

- `docs/LIVE_INFRA_RUNBOOK.md` - live fleet infra hardening runbook.
- `docs/macos-nodes-mesh-loss-2026-09-25.md` - macOS node mesh loss root cause and remediation.
- `docs/ingress-coldstart-affinity-design-2026-09-25.md` - ingress cold-start affinity design.
- `docs/dedicated-deployment-ip-assessment.md` - dedicated public IP per deployment assessment.
- `docs/blocked-work.md` - blocked/carried work re-homed from `.gm/prd.yml`.
- `docs/browser-node-fs-persistence.md` - browser node guest filesystem persistence contract.
- `docs/browser-node-proposal.md` - browser-node edge-function substrate proposal.
- `docs/browser-node-measurements.md` and `docs/browser-node-measurements.json` - live substrate
  benchmark backing that proposal.

## Scope note

The Vitra whitepaper (`pfrazee/vitra`, `docs/whitepaper.md` in that upstream repository) is an
EXTERNAL reference. There is no `docs/whitepaper.md` in this repository;
`docs/verifiable-database.md` section 9 maps its sections to the code here.
