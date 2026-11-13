# Smart contracts in hive: contract-as-data and execution transparency

Status: **implemented and live.** This describes code that is in the tree and wired onto live
managed-database mutation paths. History of the commits that built it lives in `git log` and
`.gm/prd/`; this document is present tense, not a design essay. Every claim cites `path:line`.
Where `AGENTS.md` or an older PRD row disagrees with the tree, the tree wins and the disagreement
is called out in section 6.

Scope: `crates/hive-contract` (the contract layer) and its single platform seam,
`crates/hive-cloud/src/db_contract.rs`. It does NOT cover the browser-replicated cr-sqlite
database: that is a different use of the word "contract" (a written specification), documented
in `docs/browser-db-contract.md`.

Document map:

- `docs/smart-contracts.md` (this file): what a smart contract is here, the declared op table, engine.
- `docs/verifiable-database.md`: the verifiable log, proofs, adjudication, and its trust limits.
- `docs/contract-verification.md`: the eight verification binaries and how to run them on real state.
- `docs/contract-platform-seam.md`: how hive-cloud records writes and where authorization lives.
- `docs/README.md`: index of everything under `docs/`.

---

## 1. What "smart contract" means in this codebase

A contract here is DATA, not code. `crates/hive-contract/src/contract.rs:3-5` states it directly:
per operation kind a contract declares who may emit it (`AuthPolicy`) and whether the operation may
change contract state (`Effect`), and that is the whole smart-contract surface. There is no VM, no
guest language, no bytecode and no scripting surface. `crates/hive-contract/src/lib.rs:19-21` calls
the contract one declarative table and everything else machinery around it.

The model is Vitra / Execution Transparency (`pfrazee/vitra`, `docs/whitepaper.md` in that
upstream repository). Participants append operations to an oplog, one executor applies them into
an index through a pure function, and any third party can replay the log and detect a dishonest
executor as a reproducible breach (`crates/hive-contract/src/lib.rs:4-8`). The whitepaper is an
EXTERNAL document: there is no `docs/whitepaper.md` in this repository. See
`docs/verifiable-database.md` section 9 for the section-by-section mapping.

Four things this layer is explicitly NOT (`crates/hive-contract/src/lib.rs:10-17`):

- an authorization system: tenancy stays in the platform's JWT/API-key claim and ownership check;
- a replacement for `state.json` + `store_sync`: canonical state is still the platform's;
- a GuardianDB replacement: the relational mirror stays a read-only projection;
- a blockchain or consensus log: there is no vote, quorum, global ordering or leader election over
  log contents (`crates/hive-contract/src/replicate.rs:3-9`).

## 2. The declared contract: `hive.managed-db.v1`

`Contract` is `{ id: String, version: u32, ops: BTreeMap<String, OpSpec> }`
(`crates/hive-contract/src/contract.rs:107-113`). An `OpSpec` is
`{ auth: AuthPolicy, effect: Effect, payload_keys: BTreeSet<String>, target_key: String }`
(`:99-105`). `AuthPolicy` has THREE variants: `TenantAuthorized | OpenParticipation |
PlatformAttested` (`contract.rs`, enum `AuthPolicy`); `Effect` is `StateWrite | LogOnly`.
`PlatformAttested` exists so platform-measured metering and billing events can be `StateWrite`
WITHOUT the forbidden `OpenParticipation` + `StateWrite` pair: `validate` requires
`Caller::platform_attested`, a server-derived tenant that is not `__untagged__`, and an ownership
verdict. Giving a charge open participation would let any node mint one.

The instance is one const table, `DECLARED_OPS`
(`crates/hive-contract/src/contracts/managed_database.rs:60-89`), built by
`managed_db_contract()` (`:96-104`). `CONTRACT_ID` is `hive.managed-db.v1` (`:25`);
`CONTRACT_VERSION` and `CONTRACT_SCHEMA_VERSION` are both 1, asserted equal at `:36`. Bumping the
id renames the contract and stops old replicas merging (`MergeError::ContractMismatch`).

| kind | AuthPolicy | Effect | payload keys | target key |
|---|---|---|---|---|
| `db.create` | `TenantAuthorized` | `StateWrite` | `db_id, kind, project` | `db_id` |
| `db.delete` | `TenantAuthorized` | `StateWrite` | `db_id, project` | `db_id` |
| `db.write` | `TenantAuthorized` | `StateWrite` | `bytes, db_id, project` | `db_id` |
| `db.propose` | `OpenParticipation` | `LogOnly` | `db_id, note` | `db_id` |

The payload key set is a closed schema: `validate` refuses both a missing and an unexpected key
(`contract.rs:250-270`) and requires the payload value at `target_key` to equal the submitted
target (`:275-283`). `db.propose` is the only permissionless row and is `LogOnly`; the platform
seam does not emit it.

**The guard is a runtime check, rechecked at every use.** `Contract::declare` refuses
`OpenParticipation` + `StateWrite`, and `Contract::assert_admissible()` (one shared predicate
in `contract.rs`) rechecks the WHOLE contract on every construction route: `validate`,
`Executor::commit`, `Engine::submit` and `Engine::register`/`ensure` all call it, with a typed
`OpError` / `ExecError` / `StoreError::InadmissibleContract` refusal and no panic. `validate` and
`commit` additionally recheck the exact pair on the row being used. `Contract` and `OpSpec` still
have public fields and derive `Deserialize`, so this is a RUNTIME invariant, never "unrepresentable
by construction". Witness: `contract-billing` probes G1/G2/G3 deserialize a crafted
`{"auth":"OpenParticipation","effect":"StateWrite"}` contract and prove all three refusals with
the index hash unchanged. Do not weaken those rechecks.

### 2b. The second contract: `hive.billing-meter.v1`

`crates/hive-contract/src/contracts/billing_meter.rs` is a second declarative table, built by
`billing_meter_contract()`. Its three rows:

| kind | AuthPolicy | Effect | payload keys | target key |
|---|---|---|---|---|
| `meter.sample` | `PlatformAttested` | `StateWrite` | tenant_tag, window_end_ms + the 12 `UsageTotals` SKUs | tenant_tag |
| `billing.ledger` | `PlatformAttested` | `StateWrite` | tenant_tag, entry_id, entry_ts_ms, entry_kind, amount_cents, balance_after_cents, note_hash | tenant_tag |
| `billing.checkpoint` | `PlatformAttested` | `StateWrite` | tenant_tag, period_start_ms, period_end_ms, entry_count, commitment | tenant_tag |

`exec::apply` has pure arms for all three: a sample stores the cumulative value of each compute,
storage, bandwidth, resource and workflow SKU; a ledger row stores LENGTH-PREFIXED canonical bytes
of every field that makes a charge what it is; a checkpoint stores the platform's SHA-256 ledger
chain commitment and raw row count under this log's Ed25519 signature. Because the index mutation
is a pure function of the signed op, a replay can recompute the exact expected row and compare it
against canonical billing state: a rewritten amount, a deleted charge, or an erased period is a
named divergence. See `docs/contract-platform-seam.md` section 9 and the `contract-billing`
witness (R1-R6).

## 3. Submit, validate, execute, record

`Engine::submit` (`crates/hive-contract/src/engine.rs:470-555`) builds an `Op`, takes the shard
write lock, calls `ensure`, then `validate`. A gate refusal is still recorded (`commit_refusal`,
`:507-519`) with `success:false`; a success calls `Executor::commit` (`:525`) and records
(`:527`). Every outcome is logged (`:538-553`). `SubmitResult` (`:719-742`) carries `committed`,
`error`, `seq`, `index_hash`, `log_head`, `persist_error` and `proof`.

Validation gates (`contract.rs:241-299`): the kind is declared; the payload key set matches exactly
in both directions; the `target_key` value equals the submitted target; and the auth policy holds.
`TenantAuthorized` requires a non-empty, non-`UNTAGGED_TENANT` tenant and `caller.owns_target`.
`UNTAGGED_TENANT` is `__untagged__` (`contract.rs:24`), a fail-closed sentinel that never collapses
to `personal`.

`Caller` (`contract.rs:223-228`) carries the verdict the platform already bound: `tenant`, `actor`
and `owns_target`. The engine does not compute authorization (`engine.rs:457-460`) and does not run
the tenant operation: the caller supplies `outcome: Result<(), String>` (`:462-465`). `exec::apply`
(`exec.rs:58-92`) is a pure match on the four hardcoded kind strings with no clock, randomness or
I/O; an undeclared kind produces no mutation (`:90`). `index_hash` is BLAKE3 over the sorted index
map (`exec.rs:94-98`). The engine DOES do durable disk I/O (`engine.rs:144-148`, `:399`), unlike
`apply`.

## 4. Where it is wired

- Boot: `crates/hive-cloud/src/state.rs:323` (`managed_db_contract`), `:341` (`contract_engine`),
  `:872` (built from `managed_db_contract()`), `:902-911` (`Engine::open` against
  `$HIVE_DATA/contract-oplog`). `main.rs:1149` registers the contract; `:1168` starts the flush
  loop; `:1302` calls `checkpoint_now` on shutdown.
- The single seam: `crates/hive-cloud/src/db_contract.rs`. An exhaustive scan finds 31
  `db_contract::` call sites in `admin.rs`, one in `git.rs`, and exactly one `contract_engine.submit`
  in the whole of `hive-cloud`. See `docs/contract-platform-seam.md`.
- Billing-meter wiring: `billing.rs` enqueues one measured event per `ledger_push` (every credit,
  charge, plan change and renewal), per `meter_usage` tick (all twelve cumulative SKUs), and per
  finalized `LedgerCheckpoint` at invoice rollover; `usage_metering::spawn_saver` drains them in
  order through `db_contract::drain_metering` on the bulkhead runtime. Charging itself is STILL
  off until `HIVE_BILLING_METER_RESOURCES=1`, and the single
  `leadership::may_act(BillingMeter)` writer is untouched.
- Wiring evidence: `crates/hive-contract/src/reconcile.rs:100-128` `DECLARED_SCOPE` has 16
  entries, all `wired: true`, and `measure()` (`:178-212`) reads the tree rather than trusting the
  numbers. The older PRD row `contract-unwired-declared-6-wired-0` (and its `declared=6 wired=0`
  claim) is STALE: do not cite it as current state.

## 5. What this layer does not give you

- No prevention of unauthorized writes. The platform mutates first and records afterwards; a
  REFUSED entry is a record that a write was unattested (`db_contract.rs:9-33`).
- No finality, freshness or global ordering. An inclusion proof says an entry WAS published under
  this key, never that the log still ends there (`inclusion.rs:31-34`).
- No bond, stake, slashing, challenge window, reputation or incentive
  (`crates/hive-contract/src/fraud.rs:26-28`, `appendonly.rs:25-27`). Adjudication returns a
  verdict with no consequence. Whitepaper section 4.4 leaves incentive models unspecified.
- No consensus. Honesty is enforced by replay and audit, not agreement (`replicate.rs:3-9`).
- Not an external anchor: a proof is only as good as the pinned identity it is checked against;
  with no pinned key, adjudication returns `Inadmissible`, never `Dishonest` (`audit.rs:321-326`).
- NOT freshness, NOT a completeness proof over the physical filesystem, NOT a clock. An inclusion
  proof says the entry WAS published; it does not say the log still ends there
  (`inclusion.rs`). Three measured residual trust limits, stated rather than papered over:
  `persist.rs` `.unwrap_or_default()` on a hidden/missing segment can let an omitted SEGMENT pass
  replay; `checkpoint.rs` signs `ts_ms` but no verifier validates time, so back-dating is not
  detected by this layer; `persist.rs` `sort_by_key` normalises physical reordering and
  `replicate.rs` does not compare `base_prev`, so physical reordering of segment files is invisible
  to a logical replay. The billing ledger's own `compute_ledger_checkpoint` is an UNSIGNED SHA-256
  chain (`billing.rs`) and raw rows are pruned after a period rollover: retention of the raw rows is
  a control gap, partially mitigated by the signed `billing.checkpoint` entry. Do NOT claim replay
  detects every omission, duplication, reorder or back-date.

## 6. Claims corrected against the tree

An adversarial read of `AGENTS.md:179-198` found these overstatements; `docs/` states the code,
and `AGENTS.md` has been corrected to match. Full evidence is in the PRD rows named below.

1. "unrepresentable by construction" -> runtime guard. NOW rechecked at every use by
   `Contract::assert_admissible` (validate, commit, submit, register), witnessed by
   `contract-billing` G1/G2/G3; never describe it as a type-level guarantee.
2. "seam does NOT mutate" -> `wrote` maintains the canonical write counter via `record_write`
   (`db_contract.rs:273`). It does not perform the tenant's storage write.
3. "a caller's outcome is a MEASUREMENT, never `Ok(())`" -> true for the `db.write` lanes only; `created`
   forwards its `outcome` parameter (`db_contract.rs:186-206`); two of its three call sites pass `Ok(())` (`admin.rs:13203`,
   `:13257`) and one a real `Err` (`admin.rs:13234-13240`). `purged`/`deleted` pass literal `Ok(())` (`db_contract.rs:373`, `:396`).
4. "checkpoint close never runs inline in `submit`, `db_contract::wrote` awaits it" -> normal path
   defers to the engine drain; an inline fallback fires when deferred closes exceed
   `MAX_DEFERRED_CLOSES` (1), rebuild repair closes inline, and `wrote` awaits `submit`, not the
   close (`persist.rs:96-103`, `:806-814`; `engine.rs:327-331`, `:480-482`).
5. "`Ack::canonical` folds the ack into the signed preimage" -> `entry_sig_preimage` does that;
   `Ack::canonical` only serializes the ack (`persist.rs:215-221`).
6. "never dropped" -> an unattributed record is always attempted, but `record` returns `None` with
   no entry when the store cannot be opened (`db_contract.rs:175-180`).

Confirmed as written: no consensus, no bond/incentive model, `Inadmissible` with no pinned
identity, length-prefixed canonical bytes, and ack binding against signature replay.

## 7. SOC 1 / SOC 2: control evidence, never a code state

SOC 1 (Type 1 and Type 2) and SOC 2 (Type 1 and Type 2) are EXTERNAL ATTESTATIONS performed by
an independent auditor over a stated examination PERIOD. No code state, signature or test run is
"100% SOC compliant"; a Type 2 opinion in particular requires operating effectiveness observed
over time. What this layer SHIPS is control EVIDENCE an auditor can sample, not certification:

- completeness: every `ledger_push` and meter tick enqueues an event; the bounded queue counts and
  WARNs drops instead of losing them silently; `reconcile::measure` reads the tree for wiring;
- accuracy: the twelve cumulative SKUs, the charge amount, balance, kind, timestamp and note digest
  are each length-prefixed into a signed entry; `RATE_CARD.assert_profitable()` gates charging at
  >=6000 bps gross margin against `COST_CARD`;
- cutoff: `window_end_ms`, `entry_ts_ms` and `period_start_ms`/`period_end_ms` are in the signed
  payload, and `billing.checkpoint` anchors the period boundary;
- authorization: `AuthPolicy::PlatformAttested` + `validate` fail closed without a platform
  attestation, a server-derived tenant and an ownership verdict; charging stays behind
  `HIVE_BILLING_METER_RESOURCES` and one `may_act(BillingMeter)` writer;
- change management and monitoring: contract declarations are a single diffable table; replay,
  inclusion proofs and fraud adjudication are executable; drain degradation is logged;
- retention: raw ledger rows are PRUNED after rollover. This is an OPEN evidence-retention gap;
  the signed `billing.checkpoint` entry preserves the commitment and row count but not every raw
  row. Retention policy and the auditor's sampling period remain operator decisions.

Residual trust limits above (missing segment, unvalidated clock, physical reorder, unsigned
platform ledger chain) are part of the control description, not hidden exceptions.

## 8. Open rows

`contract-checkpoint-domain-unused` (domain-tagging the checkpoint signature would invalidate
every checkpoint already on disk: HELD pending a versioned migration),
`contract-cli-documented-flags-unimplemented`, `agents-md-contract-section-stale`, plus the
pre-existing `contract-declared-scope-realtime` and
`contract-reconcile-harness-panics-on-untagged-row`.
Several older rows (`contract-fraud-proofs-absent`,
`contract-inclusion-proof-claimed-not-implemented`, `contract-no-persistence-log-lost-on-restart`,
`contract-unbounded-log-growth-no-checkpoint`) describe a state the tree has since outgrown; they
are not a description of HEAD.

---

See also: `docs/verifiable-database.md`, `docs/contract-verification.md`,
`docs/contract-platform-seam.md`, `docs/browser-db-contract.md` (different contract), and
`docs/leaderless-migration.md` (control-plane consensus, an unrelated sense of the word).
