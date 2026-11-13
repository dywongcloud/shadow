# Narwhal / Tusk for the shadow network: verdict

Status: **VERDICT ISSUED**, after the four-layer verification receipt. Research/analysis only;
no code, no roll, no push. Facts live in `docs/narwhal-consensus-facts.md` and
`docs/dogfood-dashboard-facts.md`; this document is the judgement those facts support.

Gate receipt (orchestrator's own live run, `.gm/witness-log.md`, `run=main-four-layer-20261112`):
`cargo test --workspace --locked` exit 0 (60 result lines, 444 ok, 0 failed);
`scripts/cli-acceptance.sh` exit 0 (35 passed, 0 failed);
`check-contract-bindings.py --check` exit 0 (`RESULT PASS bindings=34 layers=4 gates=36 fail_open=4
secrets_clean=1`); `git diff --check` exit 0. Also `./target/debug/contract-billing` exit 0
(`RESULT PASS probes=11 replay_ok=true signed=9 proofs=7 rewritten_detected=true
omitted_detected=true chain_tamper_detected=true`). The user's ordering sentence was "Once you have
finished and verified completion of the task above proceed with researching and analyzing if narwhal
consensus is useful"; this verdict is that later step.

---

## 1. Verdict: DO NOT ADOPT. Not as consensus, and not as a mempool, today.

Narwhal and Tusk are two different things and the user's question names only "narwhal consensus",
so both are answered separately.

- **Narwhal (the DAG mempool)** separates reliable transaction *dissemination and storage of causal
  histories* from *ordering* (arXiv abstract: "separating the task of reliable transaction
  dissemination from transaction ordering"). It supplies NO ordering authority, NO finality, NO
  fork-choice rule. It tolerates an asynchronous network; the paper quotes over 130,000 tx/sec at
  <2 s for Narwhal-HotStuff and 160,000 tx/sec at ~3 s for Tusk, scaling linearly to 600,000 tx/sec
  with more workers.
- **Tusk** is the consensus layer on top: "a zero-message overhead asynchronous consensus protocol".
  Narwhal-HotStuff is the partially synchronous alternative, with "loss of liveness can result in
  higher latency" under faults.

**Recommendation: HOLD on both.** The reason is not performance and not conservatism; it is that
neither closes a gap this platform has, while each adds a maintenance surface the codebase's own
rules reject. `docs/leaderless-migration.md:728-733` is the decisive in-tree analysis: adding a
consensus log means "operating a **third** replication substrate on top of GuardianDB/iroh-docs and
cr-sqlite, for a capability HRW + fenced lease + quorum-confirm already provides at a fraction of
the maintenance surface. Net-larger surface for zero new capability is exactly the trade this
repo's own rules reject." That document is DESIGN-ONLY (`:3`), which is a fact about its status,
not a dismissal: its argument is about substrate count, not about whether anyone implemented it.

### Where Narwhal alone would "fit": nowhere in the current topology

Narwhal would be a **fourth** dissemination substrate alongside the mesh gossip/bounded-round path,
GuardianDB (`iroh-docs`) snapshots, and `store_sync::REGISTRY` / `MERGE_STORES` /
`db_replicate::fanout_all`. Because it changes no ordering authority, it introduces no split-brain
risk and, symmetrically, **fixes no split-brain risk**. A certificate of availability would add
2f+1 attestations that >=f+1 honest validators hold the bytes (retrievability) plus a causal-history
DAG link; it would NOT add freshness, external anchoring, deterministic ordering, authorization,
or a fork-resolution rule — `docs/narwhal-consensus-facts.md:377-386`. Our inclusion proofs already
establish publication under a pinned key, replayed index hash, conflicting signed statements, and
checkpoint coverage (`inclusion.rs`, `audit.rs`, `appendonly.rs`, `checkpoint.rs`).

### Where Tusk would "fit": it is exactly the thing the tree declines

Tusk is a consensus log. `crates/hive-contract/src/replicate.rs:3` and
`docs/verifiable-database.md:175-180` state the current position: "No vote, quorum, majority, leader
election over log contents, or fork resolution by agreement". Vitra's README (independently fetched)
rules out PoW/PoS and describes one executor with many emitters, honesty by replay, and explicitly
says the software is "research and education", "not stable and should not be used in production".
A BFT commit stream over metering samples would make those samples *agreed*, not *honest*: the
producer is still the platform, and replay — not agreement — is what detects a rewritten charge.
Adding agreement would create a second, competing notion of authority. That claim was tested
adversarially: **3 valid refutations / 3 votes / 0 errors -> KILLED**
(`docs/narwhal-consensus-facts.md:417-420`).

## 2. "We never want a split brain": the honest answer

The phrase needs unpacking, because answering it with "we already have consensus" would be false.
This platform does **not** claim linearizability — `docs/leaderless-migration.md:747-750` says so
plainly ("hive does not need this healer because hive is not claiming linearizability in the first
place"). What the platform does promise is ONE writer for leader-only jobs, enforced by
`leadership::may_act(cloud, Job)`: owner AND fresh view AND not isolated AND (backup owner only)
fresh DIRECT contact with a strict majority of `HIVE_CP_VOTERS`, held for `Job::min_tenure()`
(`leadership.rs:342-418`). An owner present but unable to act is still owner everywhere, so every
job HOLDS fleet-wide. That gate is deliberately fail-closed, and "we never want split brain" is a
good description of why.

Adversarial checkers killed the claim "our current control plane has no split-brain window at all"
(**2 valid refutations / 2 votes / 0 errors**). The findings, correctly labelled, are:

| Window | Status | What it is |
|---|---|---|
| W2 two DISJOINT strict majorities over ONE agreed voter set | **REFUTED** | `has_majority` is `reached*2 > voters.len()` (`leadership.rs:228-230`); two disjoint subsets of one set cannot both exceed N/2 |
| W1 chain head + backup both pass `may_act` from different registry views | **CONDITIONAL** | enabling condition: head dropped by `region.rs:832-841` `last_seen_ms > 30 s` in B's registry but present in A's, both sides >=2 nodes, rounds still ending at the 8 s deadline, voters non-empty, partition > TTL + tenure |
| W3 differing per-node `HIVE_CP_OWNER_CHAIN` values | **CONDITIONAL** | config drift; `store_follower.rs:158-160` names the case |
| W4 conflicting authoritative GET/HEAD behind round-robin DNS | **VERIFIED** | read divergence, not two writers (`main.rs:2242-2248`, `store_sync.rs:3-11`) |
| W5 promotion-reconcile window | **CONDITIONAL** | `store_sync.rs:749-754` documents it |
| W7 empty `HIVE_CP_VOTERS` | **VERIFIED** | quorum-exempt head needs no voter majority |

My own adversarial objection was that `health::demote` HOLDS withdrawal while gossip stalls, which
should close W1. It does **not**: `bounded_round.rs:1-10` ends every round at the 8 s deadline even
with dead targets, so `stalled()` (`gossip_round.rs:104-111`, needs ~52 s with no round ENDED)
never fires; `health::demote` only flips `healthy`, and `strict_chain_owner` ignores `healthy`
(`cluster.rs:129-142`). The real removal is the `nodes()` 30 s TTL. W1 is therefore a genuine
conditional window with a concrete node/round/registry sequence
(`docs/narwhal-consensus-facts.md:230-242`), **not a proven fleet split brain** — no fleet
partition was executed in this wave, and a checker's live-code *simulation*
(`1794542735311-6888-8a42130732106f8a`) is not a fleet witness.

**Billing-ledger consequence, stated plainly.** The `x-hive-cp-epoch` fence is evaluated ONLY inside
the `x-hive-admin-forwarded` branch (`main.rs:2178-2205`), so a locally-writing owner mutates
unfenced. W1 would permit two `may_act(BillingMeter)` writers (`main.rs:4960-4966`), and ledger
rows could diverge (`LedgerEntry.id` is a random UUID, `balance_cents` read-modify-write;
`docs/leaderless-migration.md:310`). Real-money charging stays OFF by default until
`HIVE_BILLING_METER_RESOURCES=1` and `RATE_CARD.assert_profitable()` (`billing.rs:520-534`), so no
charge lands by default — but that is a mitigating default, not a proof.

**Narwhal/Tusk does not fix W1.** A 2f+1 quorum pair over ONE agreed committee intersects, but two
partitions each believing a DIFFERENT roster can each form its own 2f+1
(`docs/leaderless-migration.md:788-795`: "HRW agreement is only as good as roster agreement").
Replacing a strict owner chain with a quorum protocol would trade a HOLD for a commit stream, and
`may_act` consumes no ordering decision (`leadership.rs:342-418`) — commits would accumulate with
no job allowed to act. Under the same asynchrony a partially synchronous layer loses liveness; the
WAN timing argument is already recorded (`docs/leaderless-migration.md:751-759`, 15–30 s Raft
timeouts vs hive's 3 s cluster loop).

**The correct mitigations are local, not consensus** (filed as open rows, not implemented here,
because a fleet partition drill and a control-plane change are operator one-way doors):
1. Extend the `x-hive-cp-epoch` fence to locally-writing owners, not only forwarded mutations.
2. Complete the read-path owner proxy / `gossip::dispatch` arm / `store_sync::REGISTRY` for the
   node-local GETs in W4 — this is also the dogfood blocker.
3. Run a fleet partition drill to decide whether W1 is real; until then W1 stays CONDITIONAL.
4. Keep `HIVE_CP_OWNER_CHAIN`, `strict_chain_owner`, `may_act`, the voter-majority requirement and
   `may_adopt_wholesale_from` unchanged. Do NOT weaken the gate to "fix" a HOLD.

## 3. Billing ledger, control plane, compliance: where it all lands

- **Billing ledger**: honesty is by REPLAY, not agreement. The new `hive.billing-meter.v1` contract
  emits signed, attributed, seq-checked entries for meter samples, ledger rows and finalized
  checkpoints through the one seam, and the `contract-billing` witness proves a rewritten, omitted,
  duplicated or reordered charge is detected. A consensus layer would not improve that detection;
  it would add a second authority over the same bytes.
- **Control plane**: `admin_ingress` forwards mutations to the leader and serves GET/HEAD locally;
  wholesale store-sync adoption is only from an owner every node agrees on. A Tusk commit stream
  would sit *under* that predicate without changing it (`may_adopt_wholesale_from` is a NAME/SOURCE
  check in `cluster.rs:46-55`, not a data-availability check). That is the definition of a second
  replication substrate with no new capability.
- **SOC 1 / SOC 2**: no consensus choice is a control. `docs/compliance-controls.md` maps CC1–CC9
  and the Type 1/Type 2 examinations to concrete artefacts and states plainly that an attestation
  is an external CPA opinion over a period, not a code state. The evidence-completeness weaknesses
  it names (unsigned legacy ledger hash-chain, row pruning, four witness binaries with no
  `RESULT:` line, `contract-cost` exiting 0 on FAIL) are unchanged by Narwhal.

## 4. What this verdict is NOT

- Not a claim that the platform is free of split-brain windows. W1/W3/W5 are CONDITIONAL and open.
- Not a fleet witness. No partition was executed on the live fleet; that requires the operator's
  word.
- Not an implementation of the dogfood goal. `docs/dogfood-dashboard-facts.md` maps the
  NodeA-bound dashboard/backend path and its replication blockers; making `DashboardAPI` and
  `DashboardUI` replicated `PlatformDeployments` routed to the nearest healthy node is a production
  architecture change and remains HELD.
- Not a claim of SOC 1 / SOC 2 compliance, and not a claim that Vitra or Narwhal is production-ready
  for this platform. Vitra's own README says it is not.
- Not a change to any gate. No owner-chain, `may_act`, quorum, or wholesale-adoption predicate was
  weakened.

## 5. Sources

`https://arxiv.org/abs/2105.11827` (abstract, executed WebFetch 2026-11-12);
`https://ar5iv.labs.arxiv.org/html/2105.11827` (full paper body sections 1–9, three WebFetch
passes; all §quotes come from here). The direct PDF fetch returned a binary dump with no readable
text, and the facts doc says so rather than claiming PDF verification.
`https://github.com/pfrazee/vitra` README (independently fetched); Vitra whitepaper §2.7
(no-freshness) and §4.4 (incentive models left unspecified) as cited by the refuter lead.
In-tree: `docs/leaderless-migration.md`, `docs/verifiable-database.md`, `docs/smart-contracts.md`,
`docs/contract-verification.md`, `docs/contract-fraud-resistance.md`, `docs/compliance-controls.md`,
`AGENTS.md`, and the `file:line` cites above.
