# Verifiable database: the managed-db transparency log

Status: **implemented.** This is the verifiable half of the smart-contract layer in
`docs/smart-contracts.md`. It is a tamper-evident HISTORY of operations the platform already
authorized, not a replacement for canonical state and not a database you query. Present tense;
every claim cites `path:line`. History of the build lives in `git log` and `CHANGELOG.md`.

"Verifiable database" is not a phrase used in the source. It names the combination of: an
append-only, hash-chained oplog per contract; a signed ack per entry; a deterministic index that
any third party can replay; inclusion and append-only-violation proofs; signed checkpoint windows;
and an adjudicator that returns a verdict. What can be verified, and what cannot, is section 8.

---

## 1. The objects

| object | definition |
|---|---|
| `Op` | `seq, contract, tenant, actor, target, kind, payload` (`crates/hive-contract/src/oplog.rs:93-104`) |
| `Ack` | `seq, ts_ms, success, error, index_hash, log_head` (`oplog.rs:134-145`) |
| `LogEntry` | `op, ack, entry_hash, prev_hash` (`oplog.rs:190-197`) |
| `SegmentLine` | `{"entry": <LogEntry>, "sig": "<hex Ed25519>"}`, one JSON object per line (`persist.rs:202-207`, `:781-789`) |
| `Checkpoint` | `contract_id, contract_version, window_base, window_end, prev_head, log_head, merkle_root, index_hash, index, ts_ms` (`persist.rs:955-966`) |

The log is sharded by contract id (`engine.rs:71-84`) and opened against
`$HIVE_DATA/contract-oplog` (`crates/hive-cloud/src/state.rs:328`, `:898`, `:902-911`). Per-contract
files are `seg-<base:012>.jsonl` and `cp-<end:012>.json` (`persist.rs:224-231`). The default window
is 256 entries and 4 checkpoints are retained (`persist.rs:49`, `:54`). A restart rebuilds from the
newest VERIFIED checkpoint plus the tail and lands on the index hash it had before it stopped; a
stored checkpoint or entry that does not verify is refused, never adopted (`state.rs:328-337`).

## 2. Canonical bytes and domain separation

All digests are BLAKE3 over `domain || bytes`, lowercase 64-hex, via `fluid_core::content_hash`
(`crates/fluid-core/src/lib.rs:2857-2864`). A legacy SHA-256 identity exists for older stored
digests (`:2866-2874`); new identities are BLAKE3.

Canonical serialization is hand-rolled and independent of serde feature flags: payload and index
are `BTreeMap` (sorted by construction), never `serde_json::Value` (`oplog.rs:37-48`). Every field
is LENGTH-PREFIXED as `<len>:<bytes>` (`push_field`, `oplog.rs:67-71`):

- fields joined by `0x1f` (`oplog.rs:57-64`); a map entry is `<len>:<key>=<len>:<value>`, entries
  joined by `0x1e` (`oplog.rs:78-89`);
- `Op::canonical` joins six head fields and the payload map with `0x1d` (`oplog.rs:108-120`);
- `Ack::canonical` uses seven length-prefixed fields with an explicit `0`/`1` presence tag for
  `error` (`oplog.rs:160-181`), so `None` and `Some("")` cannot share a signature;
- `Checkpoint::canonical` uses the same encoder (`checkpoint.rs:69-81`).

A bare separator join is NOT injective: a value containing the separator or `=` reproduces the
encoding of a different field list, so two different values mint one digest and a crafted payload
travels under an entry hash the executor never signed (`oplog.rs:43-48`). Separators remain only
BETWEEN length-prefixed fields.

Domains (`oplog.rs:23-34`, `merkle.rs:27-31`, `checkpoint.rs:33`):

| domain | bytes | used for |
|---|---|---|
| `OP_DOMAIN` | `hive-contract/oplog/v1` | `entry_hash`, `Op::hash` |
| `INDEX_DOMAIN` | `hive-contract/index/v1` | `index_hash` |
| `ENTRY_SIG_DOMAIN` | `hive-contract/oplog-entry/v1` | Ed25519 entry signature preimage |
| `LEAF_DOMAIN` / `NODE_DOMAIN` / `EMPTY_DOMAIN` | `merkle.rs:27-31` | Merkle leaves, internal nodes, empty root |
| `CHECKPOINT_DOMAIN` | `hive-contract/checkpoint/v1` | declared and exported, but UNUSED: checkpoint signatures cover raw `Checkpoint::canonical()` (`checkpoint.rs:143-158`). See `contract-checkpoint-domain-unused`. |
| `SNAPSHOT_DOMAIN` | `hive-contract/snapshot/v1` | replica snapshots (`replicate.rs:47`) |

Security bindings stay classical: Ed25519 via `ring` (`crates/hive-contract/Cargo.toml:14`,
`signing.rs:139-143`), using the node's EXISTING `IntegritySigner` keypair and the same fail-closed
`open_or_create` (`crates/hive-cloud/src/state.rs:328-337`). There is no key of this crate's own.

## 3. The hash chain and the ack binding

`entry_hash` = `BLAKE3(OP_DOMAIN || prev_hash || 0x1c || op.canonical())` (`oplog.rs:295-300`,
`:305-307`). It is a pure function of `(prev, op)`, so a monitor can recompute it with no key.
`Op::hash()` (`oplog.rs:122-124`) is a DIFFERENT BLAKE3 identity, `BLAKE3(OP_DOMAIN ||
op.canonical())`, and is not the entry hash. `index_hash` = `BLAKE3(INDEX_DOMAIN ||
canonical_map(index))` (`exec.rs:94-98`).

The ack cannot be inside `entry_hash`: `ack.log_head` IS the oplog head hash produced by that entry
(`oplog.rs:143-144`, `:186-189`), which would be circular. Two acks for one op therefore produce
the same `entry_hash` and, without a binding, the same signature - an executor could hand two
monitors two different accounts of one operation with nothing to tell them apart (`oplog.rs:25-34`).

`entry_sig_preimage(entry_hash, ack)` (`persist.rs:209-221`) is
`ENTRY_SIG_DOMAIN || entry_hash || 0x1c || ack.canonical()`. `Ack::canonical` serializes the ack;
it is `entry_sig_preimage` that folds it into the signed bytes. A seen signature therefore cannot
be copied onto a conflicting ack. Sign/verify callers: `Store::append` (`persist.rs:776-780`),
`sign_entry` (`fraud.rs:79-89`), `SignedEntry::verify` (`fraud.rs:68-76`) and
`InclusionProof::verify` (`inclusion.rs:230-232`). This is the difference between a signed entry
being EVIDENCE and being a FRAUD PROOF (`oplog.rs:30-34`).

Stale code comments at `persist.rs:200-201` and `:205` describe the signature as covering only
`entry_hash`; the code is correct and the comments are not. Tracked under
`contract-checkpoint-domain-unused`.

## 4. Proofs that exist

| proof | producer / type | what it proves | verifier checks | failure |
|---|---|---|---|---|
| inclusion | `InclusionProof` (`inclusion.rs:82-91`), `prove` (`:288-306`) | entry was published by this log's key with THIS ack; optionally a Merkle path to a signed root | contract id, hash self-consistency, Ed25519 over `entry_sig_preimage`, pinned key, window (`:202-257`) | `InclusionError` (`:95-124`) |
| Merkle / window | `MerklePath` (`merkle.rs:100-109`), `merkle_prove` (`:129-157`), `WindowProof` (`inclusion.rs:54-76`) | the entry hash is the leaf at `index` in a window of `leaves` with root `root` | recomputes root, consumes all siblings (`merkle.rs:195-247`) | `OutOfRange`, `ShapeMismatch`, `RootMismatch` (`:161-170`) |
| checkpoint inclusion | `SignedCheckpoint::verify_inclusion` (`checkpoint.rs:254-312`) | the entry is inside the signed window | `covers(seq)`, shape, seq, leaf, hash, Merkle path | `CheckpointVerifyError` (`:317-365`) |
| append-only violation | `ForkedHistory` + `adjudicate_fork` (`appendonly.rs:40-45`, `:137-223`), `FraudProof::ForkedHistory` (`fraud.rs:112-121`) | two signed statements about the log at one length that cannot both be true | both checkpoints verify under the pinned identity, same-range compare, shared base, adjacent heads | `Verdict::Inadmissible` on unrelated input; `Dishonest` on a fork |
| availability / withheld history | `Availability` + `check_availability` (`appendonly.rs:49-56`, `:238-291`) | the served log covers what the executor attested | checkpoint, base attestation, tail | `Withheld` (`:60-82`): `Inadmissible`, `TailTruncated`, `UnattestedBase`, `BaseAttestationMismatch` |
| equivocation / unsupported state | `Equivocation` (`fraud.rs:99-108`), `Dispute` (`audit.rs:262-271`) | two conflicting signed publications at one seq, or an index hash no replay reproduces | signatures, seq, prefix chain, replay | `Verdict` (`audit.rs:275-288`) |

Verification WITHOUT a trusted key is STRUCTURAL ONLY (`inclusion.rs:193-196`, `:233-240`;
`checkpoint.rs:189-191`). Inclusion proofs do not indicate FRESHNESS: an unchanging proof says the
entry was published, never that it is still the head (`inclusion.rs:31-34`, `:198-201`;
`crates/hive-cloud/src/db_contract.rs:66`).

## 5. Adjudication: a verdict, not a punishment

`Verdict` is `Honest {seq, index_hash} | Dishonest {seq, published, recomputed} | Inadmissible
{reason}` (`audit.rs:274-288`), labelled `honest` / `dishonest` / `inadmissible` (`:290-298`). All
three adjudicators early-return `Inadmissible` when `trusted_public_key_hex` is `None`:
`audit::adjudicate` (`audit.rs:321-326`), `fraud::adjudicate_equivocation` (`fraud.rs:213-219`),
`appendonly::adjudicate_fork` (`appendonly.rs:142-148`). `adjudicate_fraud` (`fraud.rs:361-373`)
dispatches all three kinds. No path returns `Dishonest` with no pinned identity.

Only `Breach::DivergentIndex` produces `Dishonest`; every other error is `Inadmissible`
(`audit.rs:421-432`). `SignedCheckpoint::verify(None)` returns `Ok(())` and
`check_availability(.., None)` does not early-return `Inadmissible` - those are verifications, not
adjudications.

**There is no consequence.** `fraud.rs:26-28` states the module deliberately contains no bond,
slash, challenge period or economic mechanism; `appendonly.rs:25-27` says bonds, slashing and
challenge windows are product decisions deliberately absent. Detection is a reproducible breach
notice; there is no delivery of that notice to participants (out of scope of commit `7dc07ba3`).

## 6. Checkpoint windows and durability

A window is the unit of commitment, compaction and inclusion proof. `Checkpoint::covers(seq)` is
`window_base <= seq < window_end` (`checkpoint.rs:84-86`). `DEFAULT_CHECKPOINT_EVERY = 256`
(`persist.rs:49`), `DEFAULT_RETAIN_CHECKPOINTS = 4` (`:54`), `MAX_DEFERRED_CLOSES = 1`
(`persist.rs:96-103`).

Normal path: `Store::append` writes and signs one line and returns; on window completion it queues a
`PendingClose` (`persist.rs:743-815`). `Engine::submit` calls `start_drain()` (`engine.rs:480-482`),
which is a plain `tokio::spawn(drain_loop)` (`:174-199`); `drain_pending` runs `run_close`
(`:208-263`), which builds and signs the checkpoint, fsyncs EVERY touched segment, writes it
atomically and reclaims (`persist.rs:937-1009`). `read_window` reads every segment the range
touches, from `first = base/window*window` to `last = (end-1)/window*window`, not just the segment
the base falls in (`persist.rs:446-495`).

Two bounded exceptions to "never inline": `Store::append` calls `close_all_inline().await` when
deferred closes exceed 1 (`persist.rs:806-814`, `:837-857`), and rebuild repair calls
`store.checkpoint_now` inline inside `ensure` (`engine.rs:321-331`; `persist.rs:903-918`). Both are
documented fallbacks, not the normal per-op close. `db_contract::wrote` awaits `submit` (and thus
`store.append`), NOT the drain (`db_contract.rs:243-292`, `:126-137`). The platform's periodic
`flush()` loop IS on the bulkhead and does close windows (`crates/hive-cloud/src/main.rs:1162-1186`;
`engine.rs:651-688`).

Durability knobs: `HIVE_CONTRACT_ENTRY_SYNC` (default off) buys per-entry power-loss fsync at a
measured ~5.5 ms/op vs ~0.08 ms (`state.rs:884-896`); `HIVE_CONTRACT_FLUSH_SECS` (default 5,
minimum 1) bounds how long an entry can go unflushed (`main.rs:1168-1185`). With per-entry fsync
off, the segment is flushed on the timer, before every window close and before shutdown, so a
restart loses nothing and the exposure is bounded by an interval, never by log growth. The
per-op cost is measured by `contract-cost`; see `docs/contract-verification.md`.

## 7. Replication and honesty by replay

`Replica` is `(contract, log, index)` (`replicate.rs:52-56`). `merge(&OpLog)` (`:86-151`) is a
per-row, atomic, idempotent, ORDER-INSENSITIVE tail merge keyed by `seq`, not arrival order: a
behind peer is a no-op (`skipped_stale`, `:97-105`), a shorter non-prefix history is `Regress`
(`:108-112`), a same-length non-prefix is `Fork` (`:113-129`), otherwise the whole tail is verified
on a shadow copy before commit (`:131-150`). `verify_against` (`:157-204`) re-derives every entry
with the same pure `apply` and compares `index_hash` to the publisher's ack; a mismatch is
`DivergentAck`. A replica NEVER copies a peer's index (`:5-9`, `:153-156`); `rebuild` regenerates
it from the log via `audit::replay` (`:208-219`).

Wholesale adoption is gated: `AdoptSource::may_adopt_wholesale` (`:343-351`) admits only the chain
head, or the identity election on a chain-less mesh, exactly as the platform's store-sync rule;
from a backup owner or Fallback it degrades to per-row `merge` (`:226-227`, `:348-362`).
`ReplicaSnapshot` requires a log - an index-only sync is unrepresentable (`:297-304`).

**There is no consensus.** No vote, quorum, majority, leader election over log contents, or fork
resolution by agreement. A fork is REFUSED and "the operator has to pick one" (`:384`); a lying
ack is refused (`:393-399`). `crates/hive-contract/src/lib.rs:7-8` states it plainly: no blockchain,
no consensus, no global ordering; honesty is enforced by audit, not by agreement. The optional
`raft` feature in `crates/hive-cloud` is a CONTROL-PLANE concern, unrelated to this oplog;
`docs/leaderless-migration.md` section 6 argues against adding a consensus log to this platform.

## 8. Trust model: what is and is not verifiable

Verifiable, given a pinned identity: that a specific entry was published under that key with that
ack; that the index hash follows from replaying the log; that two signed statements about one log
length conflict; that a served log withholds an attested tail; that a checkpoint covers an entry.

NOT provided (cite these before claiming otherwise):

- consensus, agreement, global ordering or finality (`replicate.rs:3-9`, `lib.rs:7-8`,
  `engine.rs:29`);
- freshness or "this is still the head" (`inclusion.rs:31-34`, `db_contract.rs:66`);
- a bond, stake, slashing, challenge window, reputation or incentive model (`fraud.rs:26-28`,
  `appendonly.rs:25-27`); an exhaustive scan of the crate finds no such identifier;
- authorization, canonical state or transport authentication (`lib.rs:10-17`,
  `db_contract.rs:9-33`);
- an external anchor for the log head (`verify-chain-has-no-external-anchor`);
- delivery of a breach notice to participants (out of scope of `7dc07ba3`).

A signed entry is EVIDENCE, not a proof (`fraud.rs:3-8`, `:30-34`). A proof is only as good as the
identity it is checked against (`inclusion.rs:33`).

## 9. Whitepaper mapping

This crate is modelled on `pfrazee/vitra` Execution Transparency (`docs/whitepaper.md` in that
repository). That file is NOT vendored here. Verified against the upstream document on 2026-11-06:

| whitepaper | here |
|---|---|
| 2.6 audit / full audit | `audit.rs:3-5`, `:225-242` `verify`, `replay_from` (`:134-223`) |
| 2.7 inclusion proof | `inclusion.rs:1-21`, `prove`/`verify` |
| 2.4 / 2.6 append-only violation | `appendonly.rs:137-223`, `fraud.rs:112-121` |
| 3.4 index root proof at ack seq | `Ack::index_hash` (`oplog.rs:134-145`), `engine.rs:566-574` |
| 3.3 oplog pubkey in Ack | deliberately NOT implemented (`7dc07ba3`); identity is pinned out of band |
| 4.1 log availability | `appendonly.rs:238-291` |
| 4.4 incentives | deliberately unspecified upstream: "No incentive model for contract execution or monitoring is defined in this paper ... Therefore we chose to leave incentive models unspecified in this paper." |
| 6.3 multi-executor / Paxos against censorship | future work, not implemented |
| 2.3 warn participants | needs a route and gossip; not implemented |

## 10. Verification surface

The eight `contract-*` binaries, their exact arguments, result lines, exit codes and the
documented-but-unimplemented flags are in `docs/contract-verification.md`. `contract-transparency`
witnesses T1-T6 (inclusion, fork, availability, breach notice) against a real `Engine::open`;
`contract-adversary` checks that a fabricated artefact cannot convict an honest executor.

---

See also: `docs/smart-contracts.md`, `docs/contract-verification.md`,
`docs/contract-platform-seam.md`.
