# Fraud resistance: the honest threat model

Status: present tense for the tree read on 2026-11-12, with one explicitly marked
in-flight change. Every claim cites `path:line` from the LIVE working tree or a
fetched upstream source. This is a threat model, not a certification and not a
claim that the system is fraud-proof. See also `docs/verifiable-database.md`,
`docs/smart-contracts.md`, `docs/contract-platform-seam.md`,
`docs/contract-verification.md`, `docs/compliance-controls.md`.

---

## 1. What the upstream reference actually gives us

Vitra (`https://github.com/pfrazee/vitra`) calls itself "Cooperative databases using
smart contracts" and "a research project for exploring the limits of smart contracts
_without_ blockchains -- specifically, without using decentralized consensus
algorithms like Proof-of-Work or Proof-of-Stake". Its own README says "Its purpose is
research and education" and "the software is not stable and should not be used in
production". It brands the protocol "Execution Transparency (ET)".

What ET IS (README and `docs/whitepaper.md` in that repo):

- Verifiable logs via Hypercore Protocol. One executor, many participants:
  "but only one _executor_ who can execute the ops."
- Honesty by REPLAY, never agreement. "When we verify a contract, we're replaying
  the emitted operations against the apply functions to make sure the executor has
  been honest"; "a monitor validates the contract's execution by replaying the
  oplog(s) against the contract's apply function" and "The monitor then compares its
  generated index log against the executor's published index log". `verify()` is a
  one-shot audit and `monitor()` watches continuously; a replay mismatch raises
  `ContractFraudProof (The executor has violated the contract)`. Caught: an
  unprompted write, wrong changes, a skipped operation, an unpublished change.
  "By choosing instead to merely expose malicious actors, we can make a simpler,
  more efficient system."
- Inclusion proofs: sequence number, Merkle root, and a signature over the root.
  "We can use the inclusion proof to independently verify that a log message was
  published by a log, and to prove mischief if the log owner ever attempts to
  unpublish a message." "clients of an ET database can demonstrate the existence of
  a transaction to third parties."
- Append-only violation proofs from conflicting signed roots. "Violations to the
  append-only constraint are currently detected when verifying an inclusion proof";
  Hypercore `truncate()` and split feeds are violations. Forking with two different
  signed roots is "efficiently detected" if "divergent histories have been published".
- Enforcement is audit-plus-abandon: "violations are unrecoverable and will require
  users to switch to an entirely new database". Monitors "warn participants if a
  breach is found".

What ET does NOT provide:

| Boundary | Exact upstream basis |
|---|---|
| NO consensus, leader election, quorum, voting, fork-choice, finality | "without using decentralized consensus algorithms"; §2.1 "In this paper, we suggest appointing a sole authority (the executor) to ensure these properties"; §6.3 "This concern can be mitigated by introducing a Paxos-like consensus protocol for multiple executors" is stated as FUTURE work |
| NO bond, stake, collateral, slashing, reward, dispute window, challenge period | §4.4 "Incentive and Reputation Models": **"No incentive model for contract execution or monitoring is defined in this paper."** / "Incentives are important in practice but their design is orthogonal to ET's core mechanics" / **"Therefore we chose to leave incentive models unspecified in this paper."** / "No formal reputation model is specified in this paper for similar reasons." |
| Inclusion/append-only proofs show PUBLICATION, NOT freshness | §2.7 **"Note however that \"freshness\" is not indicated by inclusion proofs"**, "and so the current state of a database may need to be consulted in some cases." The README itself is UNSPECIFIED on freshness -- the caveat lives only in the linked whitepaper; do not quote it as a README line. |
| NO notary, timestamp authority or external chain anchor | Only peer comparison: §2.4 "Signed root hashes may then be compared over a gossip network to ensure equivalent histories"; "It's recommended to use a persistent gossip protocol to sync the logs to multiple third-party monitors". No anchoring primitive is described. |
| NO guaranteed availability or censorship resistance | §4.1 "If some part of history is not made available, then validation can not be accomplished"; §4.2 "it is possible for a participant's messages to be rejected or ignored without recourse" and the executor "has sole discretion over the order by which messages are processed". |
| NO transaction-result inclusion proofs, aggressive append-only detection, production runtime | README lists these as deliberately FUTURE work: "does not currently include APIs to fetch proofs for random messages in a log", "Additional append-only fraud proof detection", native-code runtime, ZK-SNARKs. |

The local crate does NOT vendor Vitra's code; it models the concepts. The mapping is
recorded at `docs/verifiable-database.md:203-218`, including §3.3 "oplog pubkey in
Ack" as deliberately NOT implemented ("identity is pinned out of band"), §6.3
multi-executor as future work, and §2.3 "warn participants" as not implemented.
The local `NOT provided` list is `docs/verifiable-database.md:188-198`, and the
stated product boundaries are `docs/smart-contracts.md:121-132`. Those are honest
trust limits, not defects and not fraud.

---

## 2. What a proof DOES prove here

- **Attribution of a stored line.** `crates/hive-contract/src/persist.rs:215-221`
  `entry_sig_preimage` signs `ENTRY_SIG_DOMAIN || entry_hash || 0x1c || Ack::canonical()`;
  `fraud.rs:68-76` `SignedEntry::verify` recomputes that preimage. `Ack::canonical`
  is the ONLY field an entry hash cannot cover, because `ack.log_head` IS that hash
  (`oplog.rs:134-155`). Canonical bytes are LENGTH-PREFIXED `<len>:<bytes>`
  (`oplog.rs:43-89`, `push_field` at `:67-71`); a bare join is NOT injective.
  `Op::canonical` includes `seq` (`oplog.rs:107-120`), so a reorder changes every
  later hash. A copied signed line with a rewritten ack therefore fails verification
  instead of convicting an honest executor (`fraud.rs:26-34`).
- **Pinned adjudication.** `fraud.rs:361-370` `adjudicate_fraud` takes
  `trusted_public_key_hex: Option<&str>`; `fraud.rs:47-54` explains that a carried
  public key is NOT self-attesting. With no pinned identity,
  `audit.rs:321-326` returns `Verdict::Inadmissible`, never `Dishonest` -- and
  `audit.rs:284-289` says explicitly that inadmissible is distinct from honest:
  a malformed accusation is not a vindication.
- **Inclusion.** `inclusion.rs:82` `InclusionProof`, `:288` `prove`, `:230`
  `verify_ed25519`, `checkpoint.rs:254` `verify_inclusion`, `merkle.rs:100`
  `MerklePath`. The proof states that an entry WAS published under a key.
- **Append-only / fork / availability / equivocation.** `appendonly.rs:38-45`
  `ForkedHistory`, `appendonly.rs:238-291` `check_availability`,
  `fraud.rs:112-121` `FraudProof` with its three variants, `fraud.rs:150-152`
  notes `ts_ms` is deliberately not part of the comparison.
- **Replay of the DECLARED op set.** `audit.rs:134-223` `replay_from` and
  `audit.rs:227-243` `verify` recompute the index; `replicate.rs:3-9` states
  "Vitra has no consensus ... honesty is enforced by replay, not by agreement".
- **Refusals are recorded, not prevented.** The seam mutates FIRST and records
  AFTERWARDS (`crates/hive-cloud/src/db_contract.rs:9-33`); `wrote` at `:243-292`
  calls `record_write` even when the contract refuses (`:273`), and
  `unattributed` at `:314-343` records a REFUSED `db.write` against
  `unattributed:<kind>:<tenant>::<name>` (`:296`, `:324`). `purged`/`deleted`
  pass a literal `Ok(())` (`:373`, `:396`); only the `db.write` lanes measure.
- **Durable, signed checkpoints.** `checkpoint.rs:69-81` signs a hand-rolled
  canonical field list; `persist.rs:103` `MAX_DEFERRED_CLOSES = 1`;
  `persist.rs:806-814` defers the close to the drain and closes inline past one;
  `persist.rs:224-231` names segments/checkpoints by zero-padded seq.
  `read_entries_from` (`persist.rs:323-349`) reads EVERY segment and filters per
  line, because "a segment's window size is not knowable from its name".

---

## 3. What a proof DOES NOT prove

1. **Not freshness.** `inclusion.rs:31-34` and `db_contract.rs:66-67` say the
   proof shows an entry WAS published, never that the log still ends there.
2. **Not completeness.** A writer holding the signing key and the canonical mirror
   can omit an event by never appending it, duplicate one, choose the commit order
   that becomes seq order, or set `ts_ms` arbitrarily, then recompute hashes, acks,
   Merkle root, checkpoint signature and canonical counter. The resulting artefact
   is self-consistent. There is no independent event inventory.
3. **Not authorization or prevention.** The contract consumes the verdict the
   platform already bound (`docs/smart-contracts.md:85-101`). A REFUSED entry is a
   RECORD, not a prevention (`docs/contract-platform-seam.md:9-33`).
4. **Not consensus, finality or global ordering.** `replicate.rs:3-9`;
   `docs/verifiable-database.md:189-190`.
5. **Not an external anchor.** `docs/verifiable-database.md:197`. A proof is only
   as good as the pinned identity it is checked against (`inclusion.rs:33`).
6. **Not economic consequence.** `fraud.rs:26-28` and `appendonly.rs:25-27`:
   "What this module deliberately does NOT contain: a bond, a slash, a challenge
   period, or any economic mechanism"; "Bonds, slashing and challenge windows are
   product decisions and are deliberately absent, exactly as 4.4 leaves them out
   of the paper". `adjudicate_fraud` returns a `Verdict`, nothing else.
7. **Not delivery of a breach notice.** `docs/verifiable-database.md:198`;
   `docs/verifiable-database.md:217` maps §2.3 to "needs a route and gossip; not
   implemented".
8. **Not time.** `checkpoint.rs:69-81` signs `ts_ms`, but no verifier validates it
   against a clock; `persist.rs:249-258` selects the "latest" checkpoint by the
   max `end` in the FILENAME, not by time. `ts_ms` occurs ZERO times in `audit.rs`
   (exhaustive literal, dispatch `1794542635101-6772-1e948c1a54a7ca6a`).

### What remains TRUSTED (there is no way around these)

| Trusted thing | Why it cannot be discharged by a proof |
|---|---|
| The PINNED identity | `adjudicate_fraud(.., None)` = `Inadmissible`, not `Dishonest` (`audit.rs:321-326`). Get the key wrong and a forged history verifies. Key resolution is fail-closed (`secrets.rs:12`, `:47`, `:211` `load_or_create_key`), rotation needs `HIVE_SECRET_KEY_OLD` (`secrets.rs:63`, `:82`). |
| The auditor's OWN replay | `audit.rs:134-223` is `f(contract, one log)`. There is no second, independent event source. An auditor that does not run it has no verdict. |
| An external anchor / second observer | ABSENT (`docs/verifiable-database.md:197`). Vitra substitutes gossip comparison of signed roots; here `replicate.rs` compares overlapping RETAINED windows. |
| Someone to ACT on a `Dishonest` verdict | No bond, no challenge window, no consequence (`fraud.rs:26-28`). Audit-plus-abandon is the only enforcement, exactly as upstream. |
| The signing key and the canonical mirror | Both are held by the writer being audited. A proof detects divergence between two artefacts the same party can mint. |

---

## 4. Adversarial verification (executed, 2026-11-12)

Three recursive checker swarms, each with two independent refuters; two valid
refutations kill a claim, a checker that errors casts no vote.

| Lens | Votes | Verdict |
|---|---|---|
| A FORGERY | **1 valid refutation / 0 errors / 1 no-vote** (strict `Verdict::Dishonest` reading: 0 valid / 0 errors / 2 no-votes) | **CLAIM SURVIVES** -- fewer than two valid refutations leave it UNVERIFIED, not killed. No path from a copied sig / rewritten ack / spliced log / unpinned or foreign key to `Verdict::Dishonest`. The one valid refutation reaches `Withheld::TailTruncated` / `Err(DivergentAck)`, i.e. an unpinned VERIFICATION finding, not an adjudication; `docs/verifiable-database.md:121` already records that distinction. Witnesses: `1794544131108-7839-2999e2a124e54387`, `1794544222532-7894-d4773cebea08f039`, `1794542291862-6368-daa9fdecfccd18d1`, `1794542392236-6498-e7f3bcef0e3f14f0` |
| B OMISSION / REPLAY | **2 valid refutations / 0 errors / 0 no-votes** | **CLAIM KILLED** |
| C COMPLIANCE THEATER | see run receipt | pending at time of writing |

Lens B killed the universal claim "no metering or billing event can be dropped,
duplicated, reordered or back-dated without replay detecting it". Witnesses:
`1794542242179-6313-adb187f7ba32954c` (`persist.rs:614 .unwrap_or_default()`),
`1794542086346-6138-bd62414a5ceacf46` (`signed_ms` only in `checkpoint.rs`),
`1794542311564-6399-e9708321aff833b6` (0 `base_prev` in `replicate.rs`),
`1794542311777-6400-6a3dc4127f07f465` (`sort_by_key` at `persist.rs:356/411/493`).
The attack matrix: drop/duplicate/reorder/back-date/hide-segment/omit-refused/
unattributed-looking-attributed/skip-segment are NOT generally detected by
`contract-audit`, `contract-reconcile` or `contract-replicate`; `reconcile` compares
two final-value index maps (`bin/contract-reconcile.rs:261`), `replicate` compares
overlapping retained windows (`bin/contract-replicate.rs:370`).

---

## 5. Metering/billing: baseline finding, in-flight change, gated status

**BASELINE FINDING (KILLED CLAIM; still TRUE of the live request path).** The
billing ledger is NOT tied to the signed contract oplog, and replay cannot see a
metering or billing event. `billing.rs` (2213 lines) had ZERO hits for
`db_contract::`, `contract_engine`, `hive_contract`
(exhaustive literal, `1794542391190-6496-b93ef56ab038ef18`,
`1794542397507-6506-22b713e98ec66aa0`, `1794542404680-6517-4b8ef0c161359cdc`);
`crates/hive-contract` had ZERO `LedgerEntry` hits
(`1794542457866-6578-bc177ddc8168e90a`); the only contract submit site in
`crates/hive-cloud` was `db_contract.rs:127-128` (`.submit(` exhaustive: 3 hits,
two unrelated, `1794542809230-6962-9b607b664382876d`). The billing ledger instead
had its OWN SHA-256 hash-chain commitment `compute_ledger_checkpoint`
(`billing.rs:932-993`), pruned raw rows (`billing.rs:1001`, `main.rs:4900`), and
`contracts/managed_database.rs:60-89` declared only `db.create`, `db.delete`,
`db.write`, `db.propose`. The replayed op set contained no metering event.

**A DECLARATIVE CONTRACT HAS BEEN AUTHORED BUT IS UNWIRED AND UNVERIFIED.**
Observed at `1794543726411-7604-985cc250ea14769b`: HEAD moved to
`5ca47f85b0d3e7528b19a985f8dc6cb8b2685886` and `git_status` showed 17 dirty
paths including a NEW untracked `crates/hive-contract/src/contracts/billing_meter.rs`
(136 lines, 5038 bytes, `1794543802889-7654-756d6c1d44f479b1`). It declares a
second contract `hive.billing-meter.v1` (`:28`), three op kinds `meter.sample` /
`billing.ledger` / `billing.checkpoint` (`:31-33`), `AuthPolicy::PlatformAttested`
+ `Effect::StateWrite` for all three (`:100-123`), all 12 `UsageTotals` SKUs in a
closed payload schema (`:39-52`, `:61-76`), and the builder goes through
`Contract::declare` (`:127-136`). `contract.rs:56` gained `PlatformAttested`,
`contract.rs:202` `assert_admissible`, `contract.rs:370` the `validate` arm;
`engine.rs:297`, `:488` call `assert_admissible`; `db_contract.rs:124` now sets
`platform_attested: false` for managed-db callers
(`1794543877531-7706-14106c124e1f5a12`).

**THE REQUIREMENT IS NOT MET YET.** Those edits are UNCOMMITTED and NOT compiled
or witnessed. Nothing emits the new op kinds: `crates/hive-cloud/src` has ZERO
hits for `billing_meter::`, `billing_meter_contract`, `meter.sample`,
`billing.checkpoint` (exhaustive, `1794543837473-7681-88f8febb82bd355c` /
`1794543844577-7689-8cd5df087d9448ff` /
`1794543824103-7667-7e6f29772cd1cc2` /
`1794543833044-7676-f41fff22b72b68ed`); literal `hive.billing-meter.v1` occurs
ONCE in the whole tree, in its own definition (`1794543799864-7653-3335ea7387f878d6`);
`git_status` shows NO edit to `main.rs` or `billing.rs`. The 14 `billing_meter`
hits in `hive-cloud` at `1794543875063-7704-9348a8dee4146cad` are ALL pre-existing
names (`HIVE_BILLING_METER_RESOURCES`, `spawn_billing_meter_loop`,
`billing_meters` snapshot/mirror, `usage_metering` docs) and NONE reference the
new contract -- treating those as wiring evidence is a false inference. A declared
contract is not an evidentiary path. The gap `OC-11` stays OPEN until the writer
lands a real emit site on the metering and ledger paths with a compile + live
`--state` witness receipt. Do not describe "billing tied to our smart contracts"
as implemented, in progress toward a verified state, or gated-on-witness only;
state plainly that it is UNWIRED.

Adding op kinds would NOT, by itself, solve the four replay limits below. A
`PlatformAttested` row is still a claim the platform makes about its own
measurement (`billing_meter.rs:10-23`), recorded after the fact, with no
independent observer.

---

## 6. Named open conditions (residuals)

Each residual is a stated open condition, not a TODO stub and not a hidden claim.

| Id | Residual | Live evidence |
|---|---|---|
| `OC-01` **no-external-anchor** | No notary/timestamp authority or third-party publication of the log head; a proof is only as good as the pinned key. Upstream substitutes gossip root comparison. | `docs/verifiable-database.md:197`; `inclusion.rs:33`; vitra whitepaper §2.4 |
| `OC-02` **no-freshness** | An inclusion/append-only proof shows publication, never that the log still ends there. README UNSPECIFIED; whitepaper §2.7 states the caveat. | `inclusion.rs:31-34`; `db_contract.rs:66-67`; `engine.rs:738-740` |
| `OC-03` **no-challenge-window-no-consequence** | A `Dishonest` verdict carries no bond, slash, expiry or punishment; enforcement is audit-plus-abandon. This is a stated PRODUCT BOUNDARY (§4.4), not a failed control. | `fraud.rs:26-28`; `appendonly.rs:25-27`; `docs/smart-contracts.md:121-132`; whitepaper §4.4 |
| `OC-04` **hidden-segment-swallowed** | `read_window(..).await.unwrap_or_default()` turns any read error into an empty window, and `if !entries.is_empty()` then skips the signed Merkle-root comparison altogether; the comment at `persist.rs:609-612` admits "when they are gone there is nothing to compare, and the signature is all that is left". | `persist.rs:604-629` (read at `1794543916959-7737-4b5c61128017ebd0`) |
| `OC-05` **unvalidated-timestamps** | `ts_ms` IS signed (`checkpoint.rs:69-81`) but no verifier validates it; `signed_ms` is a field never signed or read; `Op::canonical` carries no timestamp; `fraud.rs:150-152` excludes `ts_ms` from comparison. Back-dating is undetected. | `checkpoint.rs:69-81`, `:136`; `audit.rs` 0 `ts_ms` hits; `persist.rs:249-258` |
| `OC-06` **physical-reorder-normalised** | All three read paths `sort_by_key(|e| e.op.seq)` before any verifier sees them, so a physically reordered segment is silently normalised. | `persist.rs:356`, `:411`, `:493` |
| `OC-07` **compacted-prefix-blind** | `replicate.rs` has ZERO `base_prev` occurrences; merge compares only the OVERLAPPING retained window, so two compacted logs with different attested prefixes compare equal. `replay_from(Some(cp))` skips entries INSIDE an attested window. | `replicate.rs:86-151`, `:519-532`; `audit.rs:139-179`; `1794542311564-6399-e9708321aff833b6` |
| `OC-08` **unopenable-log-silence** | On a `submit` error `record` returns `None`, logs "contract: managed-db op NOT recorded", and the platform's mutation stands. A later persist fault sets `SubmitResult.persist_error`, but `Receipt` omits the field and the seam logs "op recorded". After restart the head is recomputed from `entries.last()`, so a shorter, perfectly consistent log replays `Ok`. | `db_contract.rs:31-33`, `:140-181`; `engine.rs:399-414`, `:493-498`, `:713-726`; `oplog.rs:339-354` |
| `OC-09` **refusal-inventory-gap** | An unattributed REFUSED write is recorded in the oplog but deliberately leaves the canonical write plane untouched, so reconciliation has no INDEPENDENT event for that lane. | `db_contract.rs:311-313`, `:324`; `docs/contract-platform-seam.md:57-90` |
| `OC-10` **tool-result-line-inconsistency** | Only FOUR of eight witness binaries print a `RESULT:`-prefixed line (adversary `:526,:529`; forge `:971,:973`; persist `:586,:588`; transparency `:708,:711`; exhaustive literal scan `1794544334463-7956`: 8 lines / 4 files). `contract-cost` prints `RESULT PASS`/`RESULT FAIL` with no colon and exits 0 even on FAIL; `contract-audit`/`contract-reconcile`/`contract-replicate` print `ALL PROBES PASSED`/`PROBE(S) FAILED` without the prefix. A monitor grepping only `RESULT:` misses four of eight. `docs/contract-verification.md:31` says "FIVE" by counting cost's colon-less line, which contradicts its own table (`:20-29`) -- a miscount, not a concealment. This is an HONESTLY DOCUMENTED tooling limit; it is a real evidence-completeness weakness for a naive monitor. | `docs/contract-verification.md:20-34`, `:116-117`; `bin/contract-cost.rs:222-224` (0 `process::exit` in 225 lines, exhaustive) |
| `OC-11` **billing-evidence-UNWIRED** | Baseline replay could not see any metering/billing event; a second declarative contract `hive.billing-meter.v1` EXISTS in the working tree but has ZERO emit sites in `hive-cloud`, no edit to `main.rs`/`billing.rs`, and is uncompiled/unwitnessed. The requirement "billing ledger tied to our smart contracts" is NOT MET. Gap OPEN until a real emit site lands with compile + live-witness receipts. | §5 above; `1794543820405-7662-f32bc00e8ab8f836`, `1794543875063-7704-9348a8dee4146cad` |
| `OC-12` **no-breach-notice-delivery** | No route or gossip disseminates a `Dishonest` verdict to affected participants; upstream §2.3 monitors "warn participants", which is mapped here as not implemented. | `docs/verifiable-database.md:198`, `:217` |
| `OC-13` **runtime-deserialize-bypass** | `Contract::declare` is a RUNTIME guard; public fields + `Deserialize` let a library caller bypass it. The live seam always passes the boot-built contract, so no production request path is affected, but a crafted `Contract` to `Engine::submit` can obtain a state mutation without an ownership verdict. The in-flight `assert_admissible()` targets this class. | `docs/smart-contracts.md:73-83`; `contract.rs:138-140`, `:202` |
| `OC-14` **disk-entry-re-signed-unverified** | `Engine::inclusion_proof` (`engine.rs:599-641`) reads a compacted entry from disk with `public_key: None` (`engine.rs:619`, `:632`), so `verify_line` early-returns without checking its signature (`persist.rs:421-428`); `inclusion::prove` then RE-SIGNS that entry with the node's real key (`engine.rs:625`, `inclusion.rs:288-306` `sign_entry`), despite `persist.rs:386` saying the "signature is re-read, never re-signed". `InclusionProof::verify` checks only `hash_is_self_consistent` (`inclusion.rs:217`), the signature and the key, so a locally tampered segment line can be laundered into a genuine node-signed `SignedEntry`. Precondition: local write access to the node's data dir, not a remote forgery. Raised by Lens A; verified live at `1794544563401-8051-1e3708b98a7ddcd0`, `1794544591499-8061-dc13470279ce9570`, `1794544617117-8071-89458bb3a043ca8`, `1794544631767-8087-d4fa64d04ee56030`, `1794544634578-8091-322042b8dacb66c4`. | `engine.rs:619`, `:625`, `:632`; `persist.rs:386`, `:421-428`; `inclusion.rs:217`, `:288-306` |

---

## 7. Bottom line

Fraud resistance here means: **a divergence between two artefacts, once both are
in an auditor's hands and the auditor has pinned the identity, is reproducible and
attributable.** It does not mean a dishonest writer is prevented, caught in real
time, punished, or unable to withhold the evidence an audit would need. Nothing in
this document asserts consensus, finality, freshness, certification, or that
charging is enabled. Charging stays OFF until `HIVE_BILLING_METER_RESOURCES=1`
(`billing.rs:523-534`, `resource_charging_enabled` at `:526`) AND every SKU clears
`MIN_GROSS_MARGIN_BPS` (`billing.rs:392`, `:507-518`); a control that HOLDS
(`leadership.rs:416` `may_act`) is not a control that passed.
