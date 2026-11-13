# Compliance controls: what the code can and cannot evidence

Status: read against the LIVE working tree on 2026-11-12 (HEAD
`5ca47f85b0d3e7528b19a985f8dc6cb8b2685886`, 17 dirty paths). Every row names a
concrete, observable artifact with `path:line` and a **dispatch id from an
executed gm run in this project**. This is a control-and-evidence inventory, NOT
an attestation, NOT a certification, and NOT a claim of compliance.

**Read this first.** A SOC 1 or SOC 2 attestation is an opinion from an independent
CPA after an examination engagement. Type 1 covers the DESIGN of controls at a
point in time; Type 2 covers design AND OPERATING EFFECTIVENESS over a stated
examination period (commonly 3-12 months), with sampled evidence, exception
tracking and a management assertion. Code can only supply controls and the
evidence those controls emit. It cannot supply the examination, the period, the
sample, the assertion or the opinion. `docs/COMPLIANCE_AUDIT.md:13-18` and `:278`
already state this plainly; this document repeats it without weakening it.
Nothing here asserts that the platform is SOC 1 / SOC 2 certified, "on the right
track for certification" in any auditor-recognised sense, or that any control has
passed an examination.

**Two flags that must never be read as passed controls.**

- Charging is OFF. `resource_charging_enabled()` at
  `crates/hive-cloud/src/billing.rs:526-534` requires BOTH an explicit operator
  opt-in `HIVE_BILLING_METER_RESOURCES=1` (or `true`) AND
  `RATE_CARD.assert_profitable().is_ok()` (`:533`, `assert_profitable` at `:509`).
  Default is FALSE: "shadow mode: meters measure, nobody is charged" (`:520-525`).
  A control that is not enabled has not operated.
- **A held control is not a passed control.** Every leader-only loop asks
  `leadership::may_act(cloud, Job)` (`leadership.rs:416`, `Job::min_tenure` at
  `:122`); an owner unable to act is still owner everywhere, so every job HOLDS
  fleet-wide (`AGENTS.md:50-56`). A HOLD is a safety outcome, not evidence that
  the control ran. Likewise `adjudicate_fraud(.., None)` returning `Inadmissible`
  (`audit.rs:321-326`) is not a passed fraud check.

Status vocabulary: **MET** = a concrete artifact exists, is enforced on a live
path, and its design evidence is observable in this tree. **PARTIAL** = artifacts
exist but scope, independent evidence, retained period evidence or a documented
limit leaves the objective uncovered. **UNMET** = no artifact, or explicitly out
of scope of code. No row is MET for Type 2 operating effectiveness: there is no
retained, time-bounded evidence set, no sampling frame, no exception log and no
CPA opinion anywhere in this repository.

---

## 1. The billing hash chain is NOT a signature (and retention is the gap)

Verified in the live tree at `crates/hive-cloud/src/billing.rs:940-993`
(`1794542509722-6637-792ac1a581a43b4f`, orchestrator-confirmed):
`compute_ledger_checkpoint` "Computes (never mutates) a tamper-evident SHA-256
hash-chain commitment over a tenant's raw" entries. It seeds a `Sha256` with the
literal domain `hive-billing-ledger-checkpoint-v1`, separates fields with `0x1F`,
sorts entries by `(ts_ms, id)`, seeds genesis with version + tenant +
`period_start_ms`, folds one leaf per entry (`id`, `tenant`, `ts_ms`, `kind`,
`amount_cents`, `balance_after_cents`, `note`), and returns
`commitment = hex(chain)` (`:993`) with `entry_count` and `pruned: false`.
Callers are `billing.rs:869` and `main.rs:4867`; the struct is at `:564-579`.

- **A hash chain is not a signature.** There is NO signature, NO signer identity,
  NO key pin and NO external anchor anywhere in that function. Anyone who can
  rewrite the ledger can recompute the chain and produce a different, equally
  well-formed commitment. Without a pinned key the commitment cannot be
  attributed to a publisher -- exactly the `Inadmissible` boundary in the contract
  layer (`crates/hive-contract/src/audit.rs:321-326` "no trusted identity was
  supplied: a verdict cannot be pinned to a publisher"). Contrast the signed
  contract oplog: `persist.rs:215-221` `entry_sig_preimage` folds the ack into
  Ed25519-signed bytes, `fraud.rs:68-76` `SignedEntry::verify` recomputes them,
  and `admin.rs:3152-3153` `integrity_signer.sign_chain_head` signs a chain head.
  The billing commitment has none of that. **Update at HEAD `c19e4526`: the
  commitment is now PUBLISHED INSIDE a signed oplog entry.** `billing.rs:870-887`
  builds a `billing.checkpoint` payload with `period_start_ms`, `period_end_ms`,
  `entry_count` and `commitment`, and `db_contract.rs:486-524` `drain_metering`
  submits it with `Caller::platform_attested = true`. That upgrades the
  commitment from an unattributed local hash to an attested entry; it does NOT
  turn the SHA-256 chain itself into a signature, does not add an external
  anchor, and does not supply a Type 2 examination period.
- **Retention is the real SOC issue.** `billing.rs:1001` `prune_ledger_range`
  removes raw entries from the in-memory ledger, `relational::prune_billing_ledger`
  removes them from the relational mirror, and `main.rs:4900` logs
  "ledger checkpoint: raw entries pruned, commitment + invoice retained". A
  checkpoint commitment survives but the entries needed to RE-DERIVE it do not.
  An auditor sampling a period cannot re-perform the computation. Mapped below as
  **S1-6 PARTIAL** and **S1-10 UNMET**.

---

## 2. SOC 1 -- controls relevant to financial reporting (metered billing)

The financial-reporting assertions are completeness, accuracy, cutoff, valuation,
authorization, segregation of duties, reconciliation and retention of the
usage-to-charge-to-invoice pipeline. `Dispatch` is an executed gm dispatch in
this project (`.gm/exec-spool/.dispatch-ledger.json`).

| Id | Objective / assertion | Concrete observable artifact (`path:line`) | T1 design | T2 period evidence | Status | Dispatch |
|---|---|---|---|---|---|---|
| S1-1 | Segregation of duties: ONE metering-to-billing writer | `leadership.rs:416` `may_act`; `main.rs:4964` `Job::BillingMeter` gate; `main.rs:4934-4953` loop doc ("one `may_act(BillingMeter)` writer"); `leadership.rs:122` `min_tenure` | MET | PARTIAL: no retained per-tick gate-decision/tenure log | **MET** | `1794542761357-6921-6d40c4f5ccec2` |
| S1-2 | Completeness & accuracy of metered usage (compute, storage, bandwidth, resource, credits) | `billing.rs:405-441` `UsageTotals` (12 cumulative dimensions, incl. `active_cpu_ms`, `mem_gb_hr_milli`, `gpu_ms`, `db_row_ops`, `db_storage_byte_ms`, `object_storage_byte_ms`, `ingress_bytes`, `egress_bytes`, `workflow_steps`); `billing.rs:1076` `meter_usage`; `main.rs:4969-5012` fleet aggregation; `billing.rs:1282` `add_credits` | PARTIAL: events are node-local numbers with NO signed oplog linkage (`OC-11`) | UNMET: charging OFF; no retained period sample | **PARTIAL** | `1794542969017-7037-67abd5a8e9e58af5` |
| S1-3 | Rate authorization / valuation floor | `billing.rs:322-335` `RATE_CARD`; `:375-388` `COST_CARD`; `:392` `MIN_GROSS_MARGIN_BPS = 6_000`; `:509-518` `assert_profitable`; `admin.rs:16601-16604` `margin_report`; `admin.rs:16531` COGS/margin INTERNAL | MET | PARTIAL: no retained rate-change approval record | **MET** | `1794542503085-6629-269e38e1834f98f7` |
| S1-4 | Plan changes and payment proof are authorized, never self-assigned | `admin.rs:11660` `apply_plan_everywhere(.., PlanChange)`; `billing.rs:1545` `confirm_checkout`; `billing.rs:812` `MAX_AMOUNT_CENTS` with bounds at `:1037`, `:1121`, `:1340` (prevents a `u64 as i64` negative) | MET | PARTIAL: no retained approval/payment-evidence index | **MET** | `1794543265250-7335-dc6b73c9b4ab80` |
| S1-5 | Cutoff: period assignment of charges and invoices | `billing.rs:932-993` `compute_ledger_checkpoint(tenant, period_start_ms, period_end_ms)` sorting `(ts_ms, id)`; `billing.rs:551-579` `LedgerCheckpoint` | PARTIAL: `ts_ms` is signed but unvalidated (`OC-05`); no external time anchor | UNMET | **PARTIAL** | `1794542431380-6559-7114ea8dbb44b435` |
| S1-6 | Ledger integrity and completeness | `billing.rs:718-727` `LedgerEntry`; `:564-579` commitment; `:940-993` unsigned SHA-256 chain (§1 above); `:1001` `prune_ledger_range`; `main.rs:4900` prune-and-mirror | PARTIAL: chain is the billing store's OWN SHA-256, NOT the Ed25519-signed contract oplog; raw rows are pruned, so a sampled period cannot be re-derived | UNMET | **PARTIAL** | `1794542509722-6637-792ac1a581a43b4f` |
| S1-7 | Reconciliation / variance detection | `reconcile.rs:100-128` `DECLARED_SCOPE` (16 wired entries), `:178-212` `measure()`; `contract-audit`/`contract-reconcile`/`contract-replicate` in `docs/contract-verification.md:97-103`; `db_contract.rs:273` `record_write` counted even on refusal; `:314-343` unattributed REFUSED record | PARTIAL: `reconcile` compares two final-value index maps (`bin/contract-reconcile.rs:261`), so a dropped or duplicated event is invisible; `OC-10` result-line gap | UNMET | **PARTIAL** | `1794542472498-6588-a6a215176f97159c` |
| S1-8 | Authorization over financial data and mutations | `auth.rs:518` `require_auth`; `:481` `is_open_route`; `roles.rs:325` `gate`; `roles.rs:72` `RULES`; `auth.rs:294` `step_up_ok` (`:551` call site); `admin.rs:442` `ct_eq` | MET | PARTIAL: no retained access-review / termination records | **MET** | `1794542079938-6130-ddafec7479addf16` |
| S1-9 | Change management over billing logic | `.github/workflows/rust-ci.yml:98` `cargo test --workspace --locked --verbose`; `:65` `python3 -I scripts/check-contract-bindings.py --check`; `:323` `./scripts/cli-acceptance.sh`; `release-npm-cli.yml:55` the same gate | PARTIAL: technical gates exist; no documented segregated approval or release authorization record | UNMET | **PARTIAL** | `1794543088380-7138-9507560b3e838726` |
| S1-10 | Retention of evidence across an examination period | `persist.rs:54` `DEFAULT_RETAIN_CHECKPOINTS = 4`; `:1025-1030` segment reclamation; `billing.rs:1001` raw-ledger pruning; `main.rs:4900` "raw entries pruned, commitment + invoice retained" | UNMET: 4 checkpoints is a compaction setting, not a retention policy; the entries needed to re-derive a commitment are deleted | UNMET: a sampled period cannot be re-performed | **UNMET** | `1794542477635-6596-70a8edb9e0055cd9` |

---

## 3. SOC 2 -- Trust Services Criteria (Type 1 design / Type 2 operation)

CC1-CC9 are the common criteria. A1 (Availability), C1 (Confidentiality) and PI1
(Processing Integrity) are the additional categories relevant to a metered
multi-tenant database. Physical-security and vendor-management criteria cannot be
satisfied by a source tree; where they appear below they are marked accordingly.

| Id | Category | Concrete observable artifact (`path:line`) | T1 design | T2 period evidence | Status | Dispatch |
|---|---|---|---|---|---|---|
| CC1 | Control environment: integrity, authority, accountability | `roles.rs:72` `RULES` (viewer < member < admin < owner); `roles.rs:325` `gate` decides a role ONCE; `auth.rs:518` `require_auth`; `teams.rs:542` `set_member_role`, `:583` roster guard; `secrets.rs:12`, `:47`, `:211` fail-closed key resolution | MET (technical layer) | PARTIAL: board oversight, HR policies and training are out of code (`docs/COMPLIANCE_AUDIT.md:13-18`) | **PARTIAL** | `1794543138897-7199-58a13143afdb5c8e` |
| CC2 | Communication and information | `docs/smart-contracts.md`, `docs/verifiable-database.md`, `docs/contract-platform-seam.md`, `docs/contract-verification.md`, `docs/contract-fraud-resistance.md`, `docs/README.md`; `CHANGELOG.md:655-672` corrected-claim history | PARTIAL: system description documented; no policy-distribution/acknowledgement record | UNMET | **PARTIAL** | `1794543375071-7432-3c3c8492cfdee94a` |
| CC3 | Risk assessment | `docs/COMPLIANCE_AUDIT.md:1-29` (104 candidates, 89 confirmed, 15 rejected); `docs/contract-fraud-resistance.md` §6 `OC-01`..`OC-13`; `.gm/prd/` ledger | PARTIAL: no risk register with named owners, likelihood and a periodic refresh date | UNMET | **PARTIAL** | `1794543354142-7405-d45c50ffd8c2000d` |
| CC4 | Monitoring activities | Eight witness binaries (`docs/contract-verification.md:20-29`); `audit.rs:227` `verify`, `:134` `replay_from`; `appendonly.rs:238-291` `check_availability` | PARTIAL: only FOUR binaries emit `RESULT:` (exhaustive literal scan:
8 lines / 4 files -- adversary:526,529; forge:971,973; persist:586,588;
transparency:708,711, dispatch `1794544334463-7956`); `docs/contract-verification.md:31`
says FIVE by counting `contract-cost`'s colon-less `RESULT PASS` as a fifth, which
contradicts its own table. `contract-cost` exits 0 even on `RESULT FAIL` and three
tools emit no `RESULT:` line (`docs/contract-verification.md:31-34`, `:116-117`, `OC-10`). This is an HONESTLY DOCUMENTED tooling limit, not concealment, but a real evidence-completeness weakness for a monitor grepping only `RESULT:`. No automated incident detection/paging (`docs/COMPLIANCE_AUDIT.md:272-276`) | UNMET: no scheduled-run log, no alert/acknowledgement trail | **PARTIAL** | `1794542005662-6044-e5324731b43050bf` |
| CC5 | Control activities | `leadership.rs:416` `may_act`; `billing.rs:509` `assert_profitable`, `:812` `MAX_AMOUNT_CENTS`; `roles.rs:325` `gate`; `admin.rs:442` `ct_eq`; `crates/hive-outbound/src/lib.rs:158` `validate_url` | MET | PARTIAL: no sampled evidence that each control operated across the period | **PARTIAL** | `1794542121383-6177-9179f7d43834aace` |
| CC6 | Logical and physical access | `auth.rs:518`, `:481`, `:294`; `secrets.rs:103` `try_decrypt`, `:211` fail-closed key, `:63`/`:82` `HIVE_SECRET_KEY_OLD`; `scripts/audit-secret-keys.sh:40-45` pre-roll check; `AGENTS.md` `env_clear()` for tenant processes | MET for logical; UNMET for physical (datacenter access not in this tree) | PARTIAL: no access-review, joiner/mover/leaver or revocation ledger | **PARTIAL** | `1794542154656-6204-2d151ff51b69f0d` |
| CC7 | System operations: detection, incidents, recovery | `main.rs:247`, `:260` `ControlledRestart::request` one chokepoint; `memwatch.rs:189` watchdog; `AGENTS.md` `KillMode=process`; `db_gateway` auth deadlines; `BoundedRound` gossip deadlines | PARTIAL: `incidents.rs` is manual CRUD only; `alerts.triggered`/`firewall.attack` declared but never dispatched (`docs/COMPLIANCE_AUDIT.md:272-276`) | UNMET | **PARTIAL** | `1794543102846-7156-a402e77e69a16417` |
| CC8 | Change management | `.github/workflows/rust-ci.yml:65`, `:98`, `:323`; `scripts/check-contract-bindings.py --check`; `git_*` verbs only (`AGENTS.md`); `scripts/hive-lockdown.sh` sole host firewall | PARTIAL: technical gates and an audited diff trail exist; no documented change approval, emergency-change log or author/approver segregation | UNMET | **PARTIAL** | `1794543096428-7147-37565edffbe32176` |
| CC9 | Risk mitigation: vendors, subprocessors, business continuity | No in-tree artifact. `docs/COMPLIANCE_AUDIT.md:278` states certification "requires an external audit engagement, written policies, employee training, and subprocessor/vendor contracts" | UNMET | UNMET | **UNMET** | `1794543356420-7408-83455baab22782b7` |
| A1 | Availability | `AGENTS.md` `may_act` HOLD semantics; `db_gateway` bounds a connection while UNPROVEN or DRAINING; `HIVE_GOSSIP_ROUND_DEADLINE_MS` / `BoundedRound`; `ControlledRestart` rate caps; `KillMode=process` | PARTIAL: mechanisms exist but no committed availability target, no measured period uptime, no capacity-plan record | UNMET | **PARTIAL** | `1794543227554-7300-2cd7a0cc098a446b` |
| C1 | Confidentiality | `secrets.rs:103` `try_decrypt`, `:211` fail-closed key, `:63` rotation; `admin.rs:442` `ct_eq`; `AGENTS.md` `env_clear()` so tenant processes inherit no `HIVE_SECRET_KEY`/`HIVE_JWT_SECRET`/`HIVE_INTERNAL_TOKEN`; `hive-outbound` refuses non-public addresses at connect | MET | PARTIAL: no data classification, retention/disposal or encryption-in-transit policy record | **MET** | `1794543239996-7316-934e2fc030d0d762` |
| PI1 | Processing integrity (metering/billing) | `billing.rs:405-441` `UsageTotals`; `:1076` `meter_usage`; `:940-993` unsigned commitment; `db_contract.rs:243-292` `wrote` measurement; `docs/smart-contracts.md:85-101` pure `exec::apply` (no clock/randomness/I/O) | PARTIAL: billing ledger UNTIED to the signed oplog (`OC-11`); a second `hive.billing-meter.v1` contract EXISTS in the working tree but is UNWIRED and UNVERIFIED; replay does not generally detect drop/duplicate/reorder/back-date | UNMET | **PARTIAL** | `1794543820405-7662-f32bc00e8ab8f836` |

---

## 4. The examination itself (both report types)

| Id | What an auditor would need | In-tree artifact | Status | Dispatch |
|---|---|---|---|---|
| AT-1 | SOC 1 / SOC 2 **Type 1**: management's system description + assertion, and a CPA opinion on the DESIGN of controls as of a date | `docs/COMPLIANCE_AUDIT.md:13-18` explicitly says certification "requires an accredited external auditor, documented organizational policies, employee security training, vendor/subprocessor contracts, and a real incident-response program -- none of which code can produce". No engagement, no assertion, no opinion. | **UNMET** | `1794543332913-7385-8f59d4dcbcff7c77` |
| AT-2 | SOC 1 / SOC 2 **Type 2**: the above PLUS operating effectiveness over a stated period, sampled by the auditor with an exception log | No period definition, no retained sample frame, no exception log, no control-owner sign-off, no opinion. `S1-10` shows raw ledger rows are pruned and only a commitment + invoice is retained. | **UNMET** | `1794543319218-7378-d2bc229dc0733324` |

**Counts (24 rows): 5 MET / 15 PARTIAL / 4 UNMET.**
MET = S1-1, S1-3, S1-4, S1-8, C1.
UNMET = S1-10, CC9, AT-1, AT-2.
The other 15 rows are PARTIAL (including CC5, whose T1 design column is MET but
whose overall row is PARTIAL because its T2 column is PARTIAL).
Every MET row is MET for DESIGN ONLY; its T2 column is PARTIAL or UNMET. No row is
MET for Type 2 operating effectiveness.

---

## 5. Stated product boundaries are not failed controls

The following are deliberately documented trust limits, not control deficiencies
and not fraud. Do not file them as UNMET controls:

- No consensus, agreement, global ordering or finality; honesty is by REPLAY
  (`crates/hive-contract/src/replicate.rs:3-9`, `docs/verifiable-database.md:189`).
- No bond, stake, slashing, challenge window, reputation or incentive model.
  Vitra whitepaper §4.4: **"No incentive model for contract execution or
  monitoring is defined in this paper."** / **"Therefore we chose to leave
  incentive models unspecified in this paper."** Local: `fraud.rs:26-28`,
  `appendonly.rs:25-27`, `docs/smart-contracts.md:121-132`. Enforcement is
  audit-plus-abandon, by design.
- No freshness: an inclusion proof says an entry WAS published, never that the
  log still ends there (`inclusion.rs:31-34`). Whitepaper §2.7 caveat; the Vitra
  README itself is UNSPECIFIED on freshness.
- No external anchor (`docs/verifiable-database.md:197`) and no breach-notice
  delivery (`:198`, `:217`).
- The seam does NOT authorize or prevent a write: the caller mutates FIRST and
  records AFTERWARDS (`docs/contract-platform-seam.md:9-33`). A REFUSED entry is
  a RECORD. This is honest, not a control failure.

The residuals that ARE open control gaps are `OC-01`..`OC-14` in
`docs/contract-fraud-resistance.md` §6; the ones that bind this matrix are
`OC-04` hidden-segment, `OC-05` unvalidated timestamps, `OC-06` physical reorder,
`OC-07` compacted-prefix blindness, `OC-08` unopenable-log silence,
`OC-10` result-line inconsistency, `OC-11` billing evidence UNWIRED, and
`OC-14` a disk entry re-signed without verifying its stored signature
(`engine.rs:619`, `:625`; `persist.rs:386`, `:421-428`; requires local write
access to the node's data dir).

---

## 6. Adversarial verification of this matrix

Three recursive checker swarms with two refuters each (two valid refutations kill
a claim; an errored checker casts no vote):

| Lens | Votes | Verdict |
|---|---|---|
| A FORGERY | **1 valid / 0 errors / 1 no-vote** (strict `Verdict::Dishonest` reading: 0 valid / 0 errors / 2 no-votes) | **CLAIM SURVIVES** -- fewer than two valid refutations leave it UNVERIFIED, not killed. The one valid refutation reaches `Withheld::TailTruncated` / `DivergentAck`, not `Verdict::Dishonest`; `docs/verifiable-database.md:121` records that as verification, not adjudication. Witnesses `1794544131108-7839-2999e2a124e54387`, `1794544222532-7894-d4773cebea08f039` |
| B OMISSION / REPLAY | **2 valid / 0 errors / 0 no-votes** | **CLAIM KILLED** -- replay does not detect a general drop/duplicate/reorder/back-date; metering/billing events are not in the replayed op set |
| C COMPLIANCE THEATER | pending at time of writing | see run receipt |

Killed claims from Lens B: (1) "no metering or billing event can be dropped,
duplicated, reordered or back-dated without replay detecting it"; (2) "the billing
ledger is tied to the signed contract oplog" -- a declarative second contract has
been authored in the working tree but has ZERO emit sites and no compile/witness
receipt, so it is UNWIRED and the requirement is NOT MET.

Lens A executed witnesses (from that swarm's own runs): `./target/debug/contract-forge`
-> `RESULT: ALL PROBES PASSED` including F13 "an accusation built by editing the
ack of a signed entry ... inadmissible" (`1794542291862-6368-daa9fdecfccd18d1`);
`./target/debug/contract-adversary` -> "honest executor convicted by a fabricated
artefact : 0" (`1794542392236-6498-e7f3bcef0e3f14f0`);
`./target/debug/contract-transparency` -> `20 / RESULT: ALL PROBES PASSED`
(`1794542756003-6916-ad95159ed5cbd639`). Note those binaries are prebuilt and
pre-date the sibling's in-flight edits, so they witness the algorithms, not the
new tree.

RESULT-line verdict (orchestrator-requested): a REAL evidence-completeness
weakness for a monitor that greps only `RESULT:` (four of eight tools missed), but
HONESTLY DOCUMENTED at `docs/contract-verification.md:31-34` and `:116-117`.
Mapped as CC4 / S1-7 PARTIAL, not as fraud and not as an UNMET control.

---

## 7. Bottom line

The code supplies a signed, attributed, replay-verifiable oplog for managed-database
writes; a single gate-guarded metering writer; a priced-with-margin rate card that
is OFF by default; a separate UNSIGNED SHA-256 billing-ledger commitment whose raw
rows are pruned; role-gated access; fail-closed secrets; and eight live witness
binaries. It does NOT supply an examination period, retained period evidence, an
external anchor, breach-notice delivery, an economic consequence for a `Dishonest`
verdict, or a wired billing-evidence path. A hash chain is not a signature and a
held control is not a passed control. Code can only make a future CPA examination
cheaper and more honest. No certification is claimed or implied.
