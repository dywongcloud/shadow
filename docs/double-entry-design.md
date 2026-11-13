# Balanced double-entry journal: design

This document records what is TRUE now. Concepts only: the VectorGuard-Labs/VectorLedger
reference is BUSL-1.1 with "Additional Use Grant: None"; no code or file from it is vendored.
Its `report.rs::is_compliant()` verdict and its file-existence probes are deliberately NOT ported.

## State placement (the load-bearing decision)

Canonical platform state is ONE in-memory representation (`state.json` + `store_sync`).
`persist.rs:244` restores with `serde_json::from_str(&s).unwrap_or_default()`: a deserialize
error is SILENT TOTAL STATE LOSS. `billing.rs`'s `snapshot()` two-tuple is also the
`store_sync::REGISTRY` wire shape for `"billing"`, decoded wholesale-replace by every follower.

Therefore the journal's canonical state is the **signed, node-local contract oplog** under
`$HIVE_DATA/contract-oplog`, in its own shard `hive.billing-journal.v1`. It is rebuilt by
`journal::Journal::replay` from `journal.entry` ops. This adds ZERO fields to
`PlatformSnapshot`, `LedgerEntry`, `BillingAccount` or the billing replication tuple: no
mixed-version reader can lose state to this change. The existing single-entry running-balance
ledger stays as a compatibility view; `journal::draft_from_legacy_row` derives a balanced
two-line draft from one legacy row without mutating it.

**Scope.** The oplog is NODE-LOCAL. A report over it evidences THAT NODE's log and names the
node and the log's signed chain tip; it is NOT fleet-wide and there is no consensus here
(Vitra honesty by replay, not agreement). Inclusion proofs show an entry WAS published, never
freshness. `admin_ingress` serves GET/HEAD locally, so a fleet-wide report would need an owner
proxy, a `gossip::dispatch` arm or `store_sync::REGISTRY`.

## Contract seam

`crates/hive-cloud/src/db_contract.rs` is the ONLY seam. The caller MUTATES FIRST (posts to the
in-memory journal) and RECORDS AFTERWARD through `Engine::submit`. A REFUSED entry is a RECORD,
not a prevention; `record_write` counts it. A `db.write` outcome is a MEASUREMENT, never a
constant `Ok(())`. Every journal op is `AuthPolicy::PlatformAttested` + `Effect::StateWrite`;
`Contract::assert_admissible` rechecks the forbidden `OpenParticipation` + `StateWrite` pair on
validate/commit/submit because `Deserialize` and public fields bypass `declare`.

Tenancy comes from the server-derived tenant tag, never `x-hive-team` and never
`__untagged__`. `db_contract::metering_owns` is the existing fail-closed predicate. No tenant
string becomes a host path; journal index ids are platform-generated sequence numbers or digests.

## Invariants enforced at the engine/type layer on EVERY write

`Journal::post` is the one writer; replay uses the SAME function. All refusals are typed
`JournalError` variants, with no panic and no silent skip. Order:

1. `TooFewLines` (>= 2 lines) before any arithmetic.
2. `IdempotencyConflict` (duplicate key).
3. `ZeroAmount` / `AmountOutOfRange`: `Amount(i64)` is a private-field newtype, but `Deserialize`
   bypasses it, so `Amount::validated()` rechecks every line. `MAX_LINE_CENTS` mirrors
   `billing::MAX_AMOUNT_CENTS`. Minor units only: `parse_minor_units` refuses any decimal,
   exponent, sign or whitespace with `CurrencyPrecision`.
4. `UnknownAccount`, `AccountClosed`, `LegalHold`, `CurrencyMismatch` (USD cents).
5. `UnbalancedEntry`: debits == credits accumulated in **i128** (no wrap), checked BEFORE any
   write.
6. `FourEyesRequired`: two distinct approvers for a large entry or a flagged account.
7. `ReversalTargetNotPosted`, `AlreadyReversed`, `ReversalLinesMismatch`,
   `CorrectionWithoutReversal`: corrections are a NEW reversal entry (flipped Dr/Cr at the
   original amount) then a NEW correction entry. The original is NEVER modified or deleted.
8. `NegativeBalance` for Asset/Expense, `ExposureLimitExceeded`, `BalanceOverflow`,
   `SuspenseNotCleared`, `GlobalLedgerEquation`:
   Sigma(Assets + Expenses) == Sigma(Liabilities + Equity + Income).
9. Strictly monotonic sequence with no gaps, and a BLAKE3 chain
   H(sequence || prev_hash || content_hash). `content_hash` covers EVERY line in order, so a
   rewritten, omitted, duplicated or reordered line changes it. Canonical bytes are
   LENGTH-PREFIXED `<len>:<bytes>` via `oplog::canonical_fields`, never a bare join.
10. `ChainBreak` / `SequenceViolation` on replay or verification.

Charging stays OFF until `HIVE_BILLING_METER_RESOURCES=1`; the ONE
`leadership::may_act(BillingMeter)` writer is unchanged. RATE_CARD > COST_CARD and
`assert_profitable` are untouched.

## Systematic efficiencies

- **Group commit:** `Journal::post_batch` holds one writer lock and appends one WORM batch
  event. Every entry still goes through `post`. The existing `drain_metering` bulkhead drain
  (MAX_DRAIN_PER_TICK) stays; the ACK-after-durability ordering is unchanged. No speedup is
  claimed without the live before/after timing in the witness output.
- **Snapshot read fence:** `Journal::fence()` + `snapshot_at(fence)` returns a VALUE at a
  sequence/head, not a lock held across writers.
- **Point lookups:** `seq_index: BTreeMap<u64, usize>` and `idempotency: BTreeMap` give O(log n)
  lookups instead of a capped full scan.
- **Independent WORM audit hash chain:** `WormChain` has its OWN BLAKE3 domain and its own
  append-only event list, covering entry posts, invariant violations, group commits,
  reconciliations and retention evaluations. HONEST LIMIT: it is a SECOND tamper-evident log
  written by the same process with the same node identity, NOT independent verification. An
  attacker controlling the writer rewrites both chains. `hive-cloud/src/audit.rs` has NO hash
  chain at all (verified by exhaustive literal codesearch: zero `prev_hash`/`blake3`/`chain_hash`
  hits); that file is outside this change's write surface and is recorded as a residual.
- **Ed25519-signed Merkle commitments over any entry range:** REUSED, not rebuilt.
  `journal::range_commitment` calls the existing `merkle_root`, `merkle_prove` and
  `SignedCheckpoint::sign`; `verify_range_commitment` recomputes the root and verifies under a
  PINNED key. No pinned identity => Inadmissible, never Dishonest.
- **Persistent immutable reconciliation records:** `ReconciliationRecord` carries run id,
  timestamp, range, entries scanned, discrepancy count, journal chain tip and WORM tip, with its
  own BLAKE3 record hash. Records are appended, never rewritten; they are also published as
  signed `journal.reconciliation` oplog entries, which is the durable copy.
- **Retention evaluation events:** `evaluate_retention` records journal lines retained and raw
  `billing_ledger` rows pruned. `billing.rs:1038 prune_ledger_range` is called from
  `main.rs:4892` right after a finalized checkpoint; `main.rs:4744`/`:5019` also retain by
  timestamp. Deleting that pruning is NOT ours to do. Journal lines are WORM-attested before
  the cursor advances, so the retained sample frame is reconstructible from the signed oplog.
  RESIDUAL: a row pruned BEFORE its signed oplog entry lands (a refused or persist-failed
  attestation) has no reconstructed sample. No fleet-wide completeness is claimed.

## Honesty limits (adversarial findings)

- **DOMAIN-ADAPTED, not the same standard.** Exhaustive `codesearch` over `crates/`
  found ZERO pre-existing chart-of-accounts, Global Ledger Equation, double-entry
  or `credit_account` code. Ten of the reference's 16 invariants had NO referent
  here (no accounts, no currency object, no account lifecycle, no second
  approver; ONE `may_act(BillingMeter)` writer; `prune_ledger_range` deletes raw
  rows). The six account types, exposure limits, legal holds and four-eyes gate
  are NEW platform-owned ADAPTATIONS, not a port of an existing standard. The
  reference's BLAKE3 chain, gapless sequence and idempotency concepts already had
  referents in `hive-contract`; the journal reuses them.
- **Balance is a WRITER SELF-CONSISTENCY ASSERTION, not fraud detection.** The
  seam mutates first and records afterwards (`db_contract.rs:10-30`) and counts
  writes even on refusal. A writer-controlled attacker posts (debit X, credit X)
  with a back-dated timestamp and a false note: the arithmetic passes, BLAKE3
  folds it and Ed25519 signs it. ZERO added writer-fraud detection. Real
  detection needs a pinned independent signer, an external anchor, or replay
  against an independently-owned log.
- **The second WORM chain is NOT independent verification.** `state.rs` says
  contract checkpoints are signed with the SAME Ed25519 keypair in the SAME file
  as the deployment integrity chain (`$HIVE_DATA/integrity_signing.key`), with no
  second key to rotate. Separate key NO, separate signer NO, separate storage NO,
  external anchor NO. It is a SECOND tamper-evident log, nothing more.
- **No compliance verdict.** The upstream reference `report.rs::is_compliant()`
  returns its verdict by construction when evidence collection never ran (7 of 8
  `rules.rs` checks are existence-only probes). Our generator filters live state
  by its OWN period range, carries `(id, hash, ts)` artifacts per control and
  emits no verdict word. No pinned identity => Inadmissible, never Dishonest.

## SOC 2 evidence (technical evidence only)

`docs/soc2-evidence-report.md` and `crates/hive-contract/src/evidence.rs` generate a report from
LIVE journal state, filtered by its OWN operating-period range, with CC1-CC9 + A1/C1/PI1,
Type 1 design vs Type 2 operating-period evidence, an Ed25519-signed Merkle commitment, the
WORM tip, reconciliation history and invariant/retention events. An independent verifier
reproduces the commitment and the chain. AT-1/AT-2 remain UNMET: no CPA opinion, no
control-owner sign-off, no operating-period examination. The report states plainly that it is
TECHNICAL EVIDENCE SUPPORTING AN ASSESSMENT, NOT an attestation, NOT a certification and NOT a
compliance claim. It never emits a compliance verdict word.
