# Contract platform seam: how managed-database writes get recorded

Status: **implemented and live.** This is the boundary between `crates/hive-cloud` and the
`hive-contract` layer described in `docs/smart-contracts.md`. Present tense; every claim cites
`path:line`. The single most important rule is in section 1: the recorder does not authorize and
does not prevent anything.

---

## 1. The rule: mutate FIRST, record AFTER

`crates/hive-cloud/src/db_contract.rs:9-33` states it: every caller here mutates first and records
afterwards - `blob_put` has already written to disk by the time `wrote` is called. A REFUSED entry
is therefore a RECORD that the platform's write was not attested, never a PREVENTION of it. The
bytes have already moved. Do not describe this layer as refusing an unauthorized write.

Authorization lives elsewhere, and the contract consumes its verdict:

- `auth::require_auth` mounted over `admin::router` (`crates/hive-cloud/src/main.rs:1701-1707`),
  with `/v1/storage/*` NOT in `is_open_route` (`auth.rs:301-314`) - an unauthenticated mutation is
  a 401 (`auth.rs:367-371`).
- `roles::gate` (`auth.rs:354-357`, `roles.rs:310`) over the `RULES` matrix; an unlisted write
  defaults to `Role::Admin` (`roles.rs:250-254`).
- ownership: `write_scope` (`admin.rs:14007-14055`), `require_project` (`admin.rs:3368`),
  `database_project_authority` (`admin.rs:13076`), `record_tenant` (`admin.rs:3338`) and the
  per-database `native_db` name/team match (`crates/hive-cloud/src/databases.rs:1175-1200`).

Tenancy comes from the cryptographically-bound JWT claim, never the spoofable `x-hive-team`
header (`admin.rs:3284-3300`); an untagged tenant is the fail-closed `UNTAGGED_TENANT` sentinel,
mirrored in `crates/hive-contract/src/contract.rs:24`.

## 2. The seam is the only one

`db_contract.rs` is the ONLY path in `hive-cloud` that calls `contract_engine.submit` for a
mutation: an exhaustive literal scan finds 31 `db_contract::` call sites in `admin.rs`, one in
`git.rs` (`purged`, `run_build` storage teardown) and one doc-comment reference in `databases.rs`.
An exhaustive `.submit(` scan in `hive-cloud` finds the managed-db `record()` and the billing-meter
`drain_metering()`, BOTH in `db_contract.rs`; the other hits are unrelated (the job scheduler and
a JS `form.submit()` string). The managed-db helpers are `pub(crate)`; the metering-mirror entry
points are `pub` because `billing.rs` and `usage_metering.rs` call them, but they are still inside
the ONE seam.

| function | line | records | can fail the request? |
|---|---|---|---|
| `actor(claims)` | `db_contract.rs:76` | nothing; a label from `claims.sub` or `unknown` | no |
| `owns(tenant)` | `:89` | nothing; helper | no |
| `record(..)` | `:104-137` | one oplog entry via `c.contract_engine.submit(&c.managed_db_contract, ..)` | no - returns `Option<Receipt>`; a store error logs and returns `None` |
| `created(..)` | `:186` | `db.create` with `kind` | no |
| `wrote(..)` | `:243-292` | `db.write`, or an unattributed record | no |
| `unattributed(..)` | `:314` | a REFUSED `db.write` | no |
| `purged(..)` | `:363` | `db.delete` under the tenant the STORED record names | no |
| `deleted(..)` | `:379` | `db.delete` | no |

`Receipt` (`:41-69`) carries the entry's seq/head/proof. Its only consumer is `database_delete`
(`admin.rs:13638-13652`), which puts `contract_seq`/`contract_head` into a 200 body and returns
`Ok(Json(body))` regardless of `None`. No `db_contract` function calls `auth::require_auth` or
`roles::gate`, and none returns a `Result` to the request path.

## 3. One nuance: what "does NOT mutate" means

The seam does NOT perform the tenant's storage write. It DOES maintain the canonical write plane:
`wrote` calls `c.databases.record_write(&db.id, &bytes)` (`db_contract.rs:273`;
`databases.rs:1216-1222`) only when `outcome.is_ok()`. That is deliberate and documented at
`db_contract.rs:256-271`: canonical state accounts for what the platform actually DID, while the
contract accounts for what it can ATTEST. A write the contract refuses therefore shows up as a
divergence - canonical has it, the contract does not - which is exactly the finding reconciliation
exists to produce. Suppressing the count would leave the unattested write with nothing to compare
against. The seam also computes an ownership verdict (`tenant_owns`, `db_contract.rs:255`) as an
INPUT to contract admission, not as platform access control.

## 4. A `db.write` outcome is a MEASUREMENT

The caller measures the write it just performed and passes `Result<(), String>` to `wrote`; an
`Err` becomes `success:false` with no index mutation (`engine.rs:462-465`). The measurements:

| kind | measured | local handler | mirrored arm |
|---|---|---|---|
| Blob | object reads back at the submitted size (`blob_object_size == size`) | `admin.rs:14077-14137` | `admin.rs:13807-13833` |
| Queue | queue depth moved (`after > before`) | `admin.rs:14242-14295` | `admin.rs:13835-13865` |
| Vector | index retains the entry (`after >= before.max(1)`, so a re-upsert still passes) | `admin.rs:14372-14430` | `admin.rs:13867-13894` |
| Pubsub | broker's PUBLISHED counter moved; `delivered` is captured but NOT used for the outcome (`admin.rs:14562-14564`): 0 live subscribers still lands | `admin.rs:14550-14604` | `admin.rs:13896-13928` |
| Realtime | `published_count` moved | `admin.rs:14657-14741` | `admin.rs:13930-13967` |

`crates/hive-contract/src/reconcile.rs:114-128` names these sites with `seam: "db_contract::wrote("`
and `measure()` reads the tree to check them. Its `line` values are hints and have drifted;
`contract-declared-scope-realtime` tracks adding the two realtime paths and fixing that drift.

**Precision:** only the `db.write` lanes measure. `created` forwards its `outcome` parameter (`db_contract.rs:186-206`); two of its three call sites pass `Ok(())`. `purged` and `deleted` pass a literal
`Ok(())` (`db_contract.rs:373`, `:396`). Do not write "every caller's
outcome is a measurement". One `created` path does pass a real `Err` (`admin.rs:13234-13240`).

## 5. Unattributed writes are recorded, not dropped

When no managed database owns the payload name, `wrote` falls through to `unattributed`
(`db_contract.rs:290`). That records a `db.write` against
`unattributed:<kind.label()>:<tenant>::<name>` (`db_contract.rs:324`, `UNATTRIBUTED` at `:296`),
with `project=""` and `owns_target=false` (`:333-335`). `owns_target=false` makes `validate`
refuse (`contract.rs:285-293`), so the entry is `commit_refusal` with no index mutation - a RECORD
that the write happened and nobody can attest it. `DbKind::label()` values are in
`databases.rs:72-84`. The prefix is a marker: a target carrying it is a STORAGE ADDRESS, never a
database id (`db_contract.rs:294-296`). It logs a WARN (`:340-349`).

Qualification: `record` returns `None` with NO entry when the engine cannot open the log
(`db_contract.rs:175-180`; `main.rs:1147-1155` logs that contract entries will NOT be recorded).
The write still stands. "Never dropped" is the intent, not an absolute guarantee under a store
failure.

## 6. Checkpoint lifecycle and the request path

A checkpoint window close must not stall a serving request. Normal path: `Store::append` queues a
`PendingClose` and returns (`persist.rs:743-815`); `Engine::submit` starts the engine drain
(`engine.rs:480-482`) and returns; `db_contract::wrote` awaits `submit`, NOT the close. The
platform's periodic `flush()` runs on `crate::bulkhead::spawn` (`main.rs:1168-1185`) and does
close windows. Two bounded inline fallbacks exist (deferred closes over `MAX_DEFERRED_CLOSES=1`
and rebuild repair). `read_window` reads EVERY segment a window's range touches. See
`docs/verifiable-database.md` section 6 for the full detail.

## 7. What this seam never does

- It never authorizes, never performs the tenant's storage mutation, and never fails the request.
- A REFUSED entry is evidence, not a prevention (`db_contract.rs:9-33`).
- It does not replace canonical state, `store_sync` or the relational mirror
  (`crates/hive-contract/src/lib.rs:13-17`).
- It does not make writes final or prove freshness (`db_contract.rs:66-67`;
  `inclusion.rs:31-34`).
- It does not impose a bond, penalty or challenge window on a dishonest executor
  (`fraud.rs:26-28`).

## 8. Verification

Run `contract-reconcile --state <state.json>` to diff the contract's index against canonical
state, and `contract-replicate --state <state.json>` to exercise peer sync. Both are described in
`docs/contract-verification.md`. `ReconcileReport` (`reconcile.rs:405-423`) reports `compared`,
`contract_only` and `divergences` of kind `MissingFromContract`, `StaleInContract` or
`ValueMismatch` (`:373-391`). `reconcile()` is a pure sorted-map diff with no I/O and no auth
(`:425-480`); it is a monitor against canonical state, not a second replay.

---

## 9. Metering and the billing ledger: the same seam, the same signed log

The managed-db functions above are not the whole seam any more. `db_contract.rs` also owns the
metering/billing mirror into the SECOND contract, `hive.billing-meter.v1` (declared in
`crates/hive-contract/src/contracts/billing_meter.rs`). The rule in section 1 is unchanged: **the
platform mutates FIRST and records AFTER.** `billing::ledger_push` has already appended the row,
`billing::meter_usage` has already moved the meter cursor, and `account()` has already finalized
the invoice before any contract event exists. A REFUSED metering entry is a RECORD that a charge
could not be attested, never a prevention of the charge.

Why a queue: `ledger_push` and `meter_usage` are synchronous and hold `BillingStore` locks, while
`Engine::submit` is async and fsyncs. Spawning one task per charge would reorder the log and lose
entries on shutdown. Instead:

1. `enqueue_meter_op(MeterOp)` (`db_contract.rs`) pushes METER SAMPLES and finalized
   `billing.checkpoint` events onto a bounded process-wide `VecDeque` (4096). Overflow is
   COUNTED (`METER_OPS_DROPPED`), WARNed with the entry id, and never silent; a transient
   `submit` failure requeues the event at the FRONT so order is preserved. Ledger rows are NOT
   enqueued here.
2. `usage_metering::spawn_saver` (`usage_metering.rs`) drains it through
   `drain_metering(&cloud)` on the `bulkhead` runtime, in queue order, at most 256 entries per
   tick. A drain-only loop remains alive even when `HIVE_USAGE_METER_SAVE_MS=0` disables the
   meter sidecar, because billing still enqueues events.
3. `mirror_ledger_rows` runs FIRST in every drain: it reads every raw `LedgerEntry` from
   canonical `BillingStore::snapshot()` and submits any id not yet in a DURABLE node-local
   cursor (`$HIVE_DATA/contract-billing-cursor.txt`). The cursor is advanced only after an entry
   with no persist error, so a restart, a crashed drain or a full queue can never permanently
   lose a charge. This is the completeness anchor, not the volatile queue.
4. `drain_metering` builds `Caller { platform_attested: true, owns_target: metering_owns(..),
   tenant: <server-derived>, actor: "platform:meter" }` and calls `contract_engine.submit`.
   `metering_owns` refuses `ANON_TENANT`, `__untagged__` and `_unattributed`, so an unattributed
   measurement is recorded as a REFUSAL with no index mutation. It never reads `x-hive-team`.
   The attestation is a platform-supplied VERDICT, exactly like `owns_target`; it is not a
   cryptographic proof, and the drain is an OBSERVER that runs on every node (charging itself
   stays behind the single `may_act(BillingMeter)` writer).

**The outcome is a MEASUREMENT, never a constant.** `mirror_ledger_rows` measures that the row
is present in the `BillingStore` snapshot it just read; `meter_usage` reads the meter cursor back
and compares it with the cumulative reading; the invoice rollover counts the raw rows in the
checkpoint's period and refuses `Ok(())` unless the count equals `cp.entry_count`. An `Err`
becomes a failed entry with no index mutation. The `billing.ledger` payload now carries the RAW
note (bounded to 256 chars) as well as its BLAKE3 digest, because
`compute_ledger_checkpoint` hashes the raw note into its SHA-256 leaf: after raw-row pruning the
period commitment is recomputable from the signed log.

What each signed entry makes replayable:

| event | carries | detects |
|---|---|---|
| `meter.sample` | all 12 cumulative `UsageTotals` SKUs + `window_end_ms` | a rewritten compute/storage/bandwidth/resource reading |
| `billing.ledger` | entry id, ts, kind, amount, balance, BLAKE3 note digest + raw note | a rewritten, omitted or duplicated charge/credit |
| `billing.checkpoint` | period bounds, raw row count, SHA-256 chain commitment | an erased period after raw-row pruning |

`exec::apply` stores the LENGTH-PREFIXED canonical bytes of each row in the contract index, and
`audit::replay` recomputes the index hash at every seq, so a monitor can recompute the exact
expected row from the signed op and diff it against canonical billing state. Witness:
`contract-billing` probes R1-R6 (replay, every entry Ed25519-signed, inclusion proofs pinned to
one identity, rewritten charge, omitted charge, drop/duplicate/reorder).

**HELD / trust limits (do not overstate).** This is evidence, not consensus and not a bond, slash
or challenge window (Vitra has none). An inclusion proof proves publication, never freshness.
`persist.rs` `.unwrap_or_default()` on a missing segment, the unvalidated `checkpoint.ts_ms`
(back-dating), `sort_by_key` normalising physical reorder, and `replicate.rs` not comparing
`base_prev` mean replay does NOT detect every physical omission/reorder/back-date. The platform
billing ledger's own `compute_ledger_checkpoint` is an UNSIGNED SHA-256 chain and its raw rows are
pruned after rollover in `main.rs` WITHOUT checking that the period was contract-attested: an
OPEN evidence-retention control gap, partially mitigated by the signed `billing.checkpoint` entry
and the durable mirror cursor. A queued `meter.sample` lost to a restart is re-attested by the
next cumulative sample; a `billing.checkpoint` lost before its drain is NOT recovered until the
next period rollover. The platform attestation is a caller-supplied verdict, not a proof. Charging remains OFF until `HIVE_BILLING_METER_RESOURCES=1` and the
single `leadership::may_act(BillingMeter)` writer is untouched; `RATE_CARD > COST_CARD` is still
asserted at >=6000 bps. SOC 1/SOC 2 (Type 1 and Type 2) are EXTERNAL ATTESTATIONS over an audit
period, never a code state: this layer ships control evidence, not certification.

---

See also: `docs/smart-contracts.md`, `docs/verifiable-database.md`,
`docs/contract-verification.md`, `docs/browser-db-contract.md` (a different contract).
