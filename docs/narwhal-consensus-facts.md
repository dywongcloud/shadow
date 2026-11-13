# Narwhal & Tusk: facts for the shadow network

FACTS ONLY — VERDICT HELD PENDING VERIFICATION

Collected 2026-11-12 by `sc3-research-lead` (SESSION_ID `hive-main-20261112-vitra-sub3`).
This wave gathers facts only. **No recommendation or verdict on whether Narwhal/Tusk is useful
for the shadow network is issued here.** The user's ordering gate is verbatim: "Once you have
finished and verified completion of the task above proceed with researching and analyzing if
narwhal consensus is useful for the shadow decentralized network as a whole taking account for
everything including our billing ledger, control plane etc and where it fits. We never want a
split brain or something of that nature". The verdict is a separate, later hop AFTER the
implementation wave's verification receipt exists. Every window below is labelled
**conditional / verified / refuted**; nothing here claims a proven fleet split brain.

## 0. Sources and what was actually executed

| Source | What was read | Status |
| --- | --- | --- |
| [arXiv abstract page](https://arxiv.org/abs/2105.11827) | Abstract + metadata, WebFetch 2026-11-12 (coordinator independently fetched the same page) | EXECUTED |
| [ar5iv full HTML rendering](https://ar5iv.labs.arxiv.org/html/2105.11827) | Entire paper body, sections 1-9 + Appendices A/B; three targeted WebFetch passes | EXECUTED |
| [arXiv PDF](https://arxiv.org/pdf/2105.11827) | WebFetch returned a raw PDF byte dump (object dictionaries, deflate streams); the reply said "**PDF text is unavailable**" and no title page, authors, or section prose were readable | FETCHED, TEXT UNUSABLE |
| `docs/leaderless-migration.md`, `docs/verifiable-database.md`, `docs/smart-contracts.md`, `CHANGELOG.md`, `AGENTS.md` | Host `Read` of located paths | EXECUTED |
| Live Rust/TS code | `gm.callers`, `gm.impact`, `gm.codeinsight outline`, `gm.codesearch literal`, then `Read` of located paths | EXECUTED |

**Therefore:** f+1/2f+1 quorums, certificate mechanics, GC, the view-change question, §8.4
limitations and §3.2/§5 quotes below are verified from the **ar5iv HTML body**, never asserted as
read from the PDF. Abstract-only claims are marked "(abstract)". WebSearch returned no usable
result blocks; no search-result source is cited. Two checker children and four refuter
grandchildren independently fetched the same ar5iv page (see §7).

## 1. The paper's actual design

**Title:** "Narwhal and Tusk: A DAG-based Mempool and Efficient BFT Consensus" (arXiv:2105.11827).
Section headings (verbatim, ar5iv HTML): 1 Introduction; 2 Overview; 2.1 System model, goals and
assumptions; 2.2 Intuitions behind the Narwhal design; 3 Narwhal Core Design; 3.1 The Narwhal
Mempool; 3.2 Using Narwhal for consensus; 3.3 Garbage Collection; 4 Building a Practical System;
4.1 Quorum-based reliable broadcast; 4.2 Scale-Out Validators; 5 Tusk asynchronous consensus;
5.1 Safety intuition; 5.2 Liveness and latency; 6 Implementation; 7 Evaluation (7.1 Narwhal as a
Mempool, 7.2 Scale-Out using many workers, 7.3 Performance under Faults); 8 Related work &
Limitations (8.1 Performance, 8.2 Asynchronous Consensus & Tusk, 8.3 DAG-based Communication &
Narwhal, 8.4 Limitations); 9 Conclusion; Appendix A Security Analysis; Appendix B Artifact
Appendix.

### 1.1 Two layers, not one

- Narwhal is a **DAG-based mempool / reliable broadcast**, not a consensus protocol. Abstract:
  "We propose separating the task of reliable transaction dissemination from transaction ordering,
  to enable high-performance Byzantine fault-tolerant quorum-based consensus"; it specializes in
  "high-throughput reliable dissemination and storage of causal histories of transactions".
  §3.2: "leaving consensus only the job of ordering small fixed-size references", "the consensus
  layer only needs to order block digest certificates", "Any consensus protocol can execute over
  the mempool by occasionally ordering certificates", "any deterministic rule over this DAG leads
  to the same total ordering of blocks for all validators".
- Tusk is the **asynchronous ordering layer on top**: abstract calls it "a zero-message overhead
  asynchronous consensus protocol"; §5: "Tusk is a fully-asynchronous, wait-free consensus",
  "by examining its local DAG without sending any additional messages", "every validator locally
  interprets its local view of the DAG and use the shared randomness to determine the total order",
  "Tusk has zero message overhead, and the same theoretical throughput as Narwhal".
- The paper also evaluates **Narwhal-HotStuff**, the *partially synchronous* composition, which is
  a third thing (abstract: "loss of liveness can result in higher latency", "intermittent loss of
  liveness due to asynchrony"). Conflating "Narwhal" with "consensus" is a category error in this
  paper's own terms.

### 1.2 Quorums and certificates (§2.1, §3.1, §4.1)

- Fault threshold §2.1: "can corrupt up to f<n/3 parties", Byzantine vs honest; a literal `n=3f+1`
  formula does not appear on the HTML page.
- §2.1 timing: "we assume asynchronous _eventually reliable_ communication links among honest
  parties", "there is no bound on message delays and there is a finite but unknown number of
  messages that can be lost".
- §2.1: "The returned value _c(d)_ represents an unforgeable _certificate of availability_ on the
  digest dd". §3.1/§4.1: "Once the creator gets 2f+1 distinct acknowledgments for a block" that
  include "the block digest, current round, and creator identity"; "A certificate of availability
  includes 2f+1 signatures"; "2f+1 such signatures form a certificate of availability"; "at least
  f+1 honest validators have checked and stored the block". A new block is created once
  certificates for round r-1 are accumulated from 2f+1 distinct validators (§3.1). So **2f+1 is
  the certificate / round-advance quorum; f+1 is the honest-holder count that makes the data
  retrievable**. Tusk's commit rule counts f+1 supporting blocks in a wave's second round that
  refer to the elected leader block (§5/§6 quotes reported by checker A's refuters; not
  independently re-fetched in this wave).
- §4.1 reliable broadcast relies on "perfect point-to-point channels that re-transmit the same
  message forever".

### 1.3 Asynchrony, liveness, no view-change (§5, §5.1, §5.2)

- Tusk uses a perfect shared coin: §5.2 "leverage a perfect shared coin to get a constant
  probability", "we use the randomness produced in the third round of a wave to determine the
  wave's block leader", "validators produce randomness to elect one random leader". §5.2:
  "In expectation, Tusk commits a block leader every 7 rounds in the DAG under an asynchronous
  adversary"; "In networks with random message delays, in expectation, Tusk commits each block in
  the DAG in 4.5 rounds"; "even if the adversary fully controls the network".
- **View-change:** the exact terms "view-change", "view change", "checkpoint" and "state-sync" do
  not occur in the ar5iv HTML. No leader-replacement, reconfiguration or fallback-leader
  subprotocol is described. The "leader" is chosen *retroactively* per wave from shared
  randomness, not installed by a view. Honest limit: this is an absence claim about the HTML
  rendering, not about the unreadable PDF byte dump.
- GC (§3.3): "agree on the garbage collection round", "Blocks from earlier rounds can be safely
  be stored off the main validator", "All in all, validators in Narwhal can operate with a fixed
  size memory", "re-inject transactions to a later round", "request all blocks in its causal
  history from validators that signed the certificates", "an external provider operating a Content
  Distribution Network (CDN) such as Cloudflare or S3". **No checkpoint / state-sync protocol is
  described**; catch-up is causal-history fetch from certificate signers plus optional CDN offload.

### 1.4 Measured performance (abstract, §7)

- Testbed (§7): "We deploy a testbed on Amazon Web Services, using m5.8xlarge instances across 5
  different AWS regions", "10Gbps of bandwidth, 32 virtual CPUs (16 physical core)", "each
  measurement in the graphs is the average of 2 runs", "4 validators each running with a single
  worker, a batch size of 500KB".
- Abstract: "Narwhal-Hotstuff achieves over 130,000 tx/sec at less than 2-sec latency compared
  with 1,800 tx/sec at 1-sec latency for Hotstuff"; "Additional workers increase throughput
  linearly to 600,000 tx/sec without any latency increase"; "Tusk achieves 160,000 tx/sec with
  about 3 seconds latency"; "Under faults, both protocols maintain high throughput, but
  Narwhal-HotStuff suffers from increased latency".
- §7 under faults (10 validators, 1 or 3 crash-faults): "baseline HotStuff throughput drops 5x",
  "latency increases 40x compared to no faults", "batched Hotstuff throughput drops 30x (from 70k
  tx/sec to 2.5k tx/sec)", Tusk "committing in less than 4 sec under 1 fault, and less than 6 sec
  under 3 faults", "at less than 7 sec for 1 fault, and around 10 sec for 3 faults" "(compared to
  35-40 sec for baseline or batched)".

### 1.5 Limitations and what certificates do NOT prove

- §8.4: "slow authorities are indistinguishable from faulty ones", "This creates issues around
  fairness and incentives", "Narwhal relies on clients to re-submit a transaction if it is not
  sequenced in time", "The execution engine now needs to locate and retriever transaction data from
  the various workers", "Building an efficient execution engine for Narwhal is deferred to future
  work", "Narwhal's certificates irrevocable indicate which worker holds the transaction data",
  "the high throughput of Narwhal and Tusk is only useful is there exist a transaction engine
  capable to match their speed".
- **Narwhal alone does not do state-machine replication or ordering**: §3.2 defers the execution
  engine, says any deterministic rule over the DAG works, and "using techniques from deterministic
  databases"; the application must be deterministic. Availability and ordering are deliberately
  split.
- **No external anchoring** (ar5iv whole-body read): blocks are identified solely by "The unique
  (cryptographic) digest of its contents"; §3.1 "The system is initialized through all validators
  creating and certifying empty blocks"; the CDN/S3 mention is historical-data offload only. No
  external clock, state root, checkpoint, work proof or trust anchor appears.
- **No freshness/recency**: certificates prove availability, integrity and causal inclusion.
  §8.4's certificate statement is about *which worker holds the data*; round labels are logical,
  not wall-clock, and §3.3 explicitly re-injects older transactions into later rounds. The HTML
  body makes no recency, timestamp or freshness claim.

## 2. Our control plane, from live code

All paths absolute under `/Users/dylanwong/fluid/hive`; file:line read from HEAD 2026-11-12.

| Fact | Live code |
| --- | --- |
| `HIVE_CP_OWNER_CHAIN` = first chain entry PRESENT in `registry.nodes()` with `peer_id` AND public address; `None` = HOLD; identity election and pins ignored while a chain is set | `crates/hive-cloud/src/cluster.rs:4-10, 117-127, 129-142, 172-203` |
| `nodes()` drops peers with `last_seen_ms` older than 30 s (the mechanism that actually removes a silent node) | `crates/hive-edge/src/region.rs:831-844`; `crates/hive-cloud/src/health.rs:46-58` |
| `OwnerSource::{Chain,Election,Fallback}`; `is_authority()` = Chain/Election; `may_adopt_wholesale_from` = Election true, Chain only when `chain.first() == owner`, Fallback false | `crates/hive-cloud/src/cluster.rs:26-56` |
| `may_act(cloud, Job)` = owner + fresh view + not isolated + (backup owner only) fresh DIRECT contact with strict majority of `HIVE_CP_VOTERS`, held for `Job::min_tenure()` | `crates/hive-cloud/src/leadership.rs:1-22, 195-231, 237-280, 282-309, 342-418` |
| `min_tenure`: NodeDeathRelocate 600 s, ACME 300 s, BillingMeter 120 s, DNS/GitPoll 60 s, default 30 s; `TERM_LAPSE` 30 s; take-over keys on the ownership term | `crates/hive-cloud/src/leadership.rs:77-81, 120-130, 163-166, 311-340` |
| Chain head acts on presence (no voter majority); a **backup** needs the majority; unset `HIVE_CP_VOTERS` under a chain logs ERROR and says a backup takes over "on gossip presence alone" | `crates/hive-cloud/src/leadership.rs:222-230, 469-511`; `CHANGELOG.md:2871-2877` |
| Owner present but unable to act is still owner everywhere → every job HOLDs fleet-wide; chain-less node with mesh roster HOLDs instead of electing itself | `crates/hive-cloud/src/leadership.rs:18-22, 252-266`; `crates/hive-cloud/src/cluster.rs:185-197` |
| `is_control_plane_leader()` = not isolated AND resolved authority owner == self; `leader_forward_target()` is the single forwarding resolution | `crates/hive-cloud/src/state.rs:594-659` |
| `mesh_isolated` = expected peers > 0 AND zero directly reachable healthy peers; a partition with >=2 nodes per side is NOT isolated | `crates/hive-cloud/src/state.rs:490-524` |
| `admin_ingress`: MUTATIONS forward to leader, GET/HEAD (and `/v1/token`, owner-routed paths) served LOCALLY | `crates/hive-cloud/src/main.rs:2162-2249` |
| `x-hive-cp-epoch` fence is checked ONLY inside the `x-hive-admin-forwarded` branch; forwarded mutations carry it; local serves never check it | `crates/hive-cloud/src/main.rs:2178-2205, 2414-2417`; `crates/hive-cloud/src/cluster.rs:12-16, 103-109` |
| Store-sync adoption is WHOLESALE-REPLACE; only from chain HEAD / chain-less election; backup or Fallback = per-row `MERGE_STORES` only; `Target::still_permitted` re-checks before and after a pull | `crates/hive-cloud/src/store_follower.rs:24-25, 121-148, 151-191`; `crates/hive-cloud/src/store_sync.rs:1-24, 39, 530` |
| Promotion reconcile: `admin_ingress` starts serving writes as soon as `is_control_plane_leader()` flips, with no coordination with the fetch; requires 2 independent byte-identical richer peer snapshots | `crates/hive-cloud/src/store_sync.rs:749-771` |
| Gossip: `dispatch` arms longest-prefix-first; unknown path = EMPTY body; forwarded call served locally, never re-proxied; store-snapshot arm | `crates/hive-cloud/src/gossip.rs:11-20, 402-412, 744, 1756` |
| One gossip-round deadline (default 8000 ms) through `BoundedRound`; straggler leash 2500 ms after 2 missed deadlines, PARKED never cancelled; `stalled()` = no round ENDED for 4 worst-case periods | `crates/hive-cloud/src/gossip_round.rs:1-23, 31-50, 68-90, 99-111`; `crates/hive-cloud/src/bounded_round.rs:1-17` |
| `health::demote` withdraws only after `DEMOTE_STALE_ROUNDS`=2 consecutive stale round ends and never while the loop is not ending rounds; HOLDs while stalled | `crates/hive-cloud/src/health.rs:1-38, 59-65, 99-141, 229-290` |
| ONE `may_act(BillingMeter)` writer; charging OFF until `HIVE_BILLING_METER_RESOURCES=1` AND `RATE_CARD.assert_profitable()`; `RATE_CARD` > `COST_CARD` (MIN_GROSS_MARGIN_BPS) | `crates/hive-cloud/src/main.rs:4955-4966, 1625-1633`; `crates/hive-cloud/src/billing.rs:322-335, 375, 509-517, 520-534` |
| Canonical state = ONE in-memory representation (`state.json` + `store_sync`), never GuardianDB / SQL mirror; `persist::load()` silently `unwrap_or_default()` on a deserialize error | `crates/hive-cloud/src/persist.rs:148-153, 241-247`; `crates/hive-cloud/src/store_sync.rs:1-24`; `crates/hive-contract/src/lib.rs:15-16` |
| Vitra has NO consensus: honesty by REPLAY; inclusion/append-only violation proofs show publication, not freshness; no bond/incentive model; fork is REFUSED, "the operator has to pick one" | `crates/hive-contract/src/lib.rs:7-8`; `crates/hive-contract/src/replicate.rs:1-39`; `docs/verifiable-database.md:170-186`; `docs/smart-contracts.md:155-156`; `AGENTS.md:204` |

## 3. The pre-existing design analysis (must be read with this doc)

`docs/leaderless-migration.md` is **status: "design only. No code in this document has been
written."** (`:3`). It is the closest prior analysis of the user's exact "never want a split brain"
question, and it is a design document, not shipped behavior:

- `:7-12`: scope is removing the single leader from 66 call sites; "It does *not* propose adding a
  consensus log (Raft/openraft/etcd) to this platform; §6 explains why".
- `:3.4 / :220-238`: three existing replication substrates — `gossip::dispatch` over QUIC
  (HTTP-shaped unicast, "**Never** as a replication log"), GuardianDB (wall-clock LWW),
  `hive-crsql` (per-column Lamport). Adding a consensus log is "operating a **third** replication
  substrate ... Net-larger surface for zero new capability" (`:728-733`).
- `:734-750`: rejects a Raft split-brain healer as "actively dangerous, do not import" (unauthenticated
  `healAck`, per-key LWW trades away the core safety guarantee) and says "hive does not need this
  healer because hive is not claiming linearizability in the first place".
- `:751-759`: Tau's 15 s/30 s heartbeat/election timeouts vs hive's 3 s cluster loop — "Tau's own
  constants are the argument against copying tau".
- `:760-767`: a monotonic voter set is a regression against hive's churn-native 30 s TTL roster.
- `:671`: the proposed end state reports `consensus: "ownership"`, i.e. it deletes authority notions
  rather than adding a second one.
- `:1.2 / :54-60`: reads were deliberately moved off the leader because "one wedged/far leader read
  as 'the platform is down' even though every node held the data".
- `:3.5 / :240-256`: "`store_sync` is not a replication mechanism and cannot be reused as one";
  wholesale replace under two writers is "mutual clobbering on a 60s tick".

**Layer distinction that follows from §1.1 + this doc:**
- Adding **Narwhal only** = a fourth data-dissemination substrate alongside `gossip::dispatch`,
  GuardianDB/iroh-docs and `store_sync`, with no ordering-authority change. By the code facts in
  §2 it introduces no new write-authority split and fixes no existing one.
- Adding **Tusk** (or Narwhal-HotStuff ordering) = adding a consensus/ordering log of exactly the
  kind `docs/leaderless-migration.md:10, 728-733` declines, i.e. a second answer to "who may
  order/write fleet state now". Checker A killed the claim that this can be done without a
  competing notion of authority (§7).

## 4. Split-brain analysis: every place two disjoint sets could each believe they may act

Status labels: **verified** = read directly from live code; **conditional** = a code-derived
window requiring a stated enabling condition, NOT live-witnessed on the fleet in this wave;
**refuted** = the arithmetic or code rules close it.

### W1 — Two self-believed chain owners under a network partition. **CONDITIONAL**

**Exact enabling condition.** With a common chain `H,B,C,D` and voters `H,B,C`, a partition into
`A={H,D}` and `B={B,C,...}` that persists longer than (30 s registry TTL + the job's tenure),
where (i) H is WITHDRAWN FROM `B`'s `registry.nodes()` while REMAINING PRESENT in `A`'s, (ii) both
sides have >=2 nodes so neither is `isolated`, (iii) both sides keep ENDING gossip rounds, and
(iv) `HIVE_CP_VOTERS` is non-empty on B's side so B can prove a 2-of-3 direct majority.

**Does HOLD-on-stalled-gossip close it? NO — and the evidence is precise.**
`gossip_round::stalled()` (`gossip_round.rs:104-111`) is true only before the loop starts or when
no round has ENDED for `4*(deadline+ROUND_SLEEP)` ~= 52 s. A `BoundedRound` ends every round at
the 8 s deadline *even when no target answers* (`bounded_round.rs:1-10`; `gossip_round.rs:68-76`),
so in a clean partition rounds keep ending and `stalled()` stays false;
`health::stale_long_enough` (`health.rs:129-141`) is therefore NOT blocked. More importantly,
`health::demote` (`health.rs:229-290`) only flips the `healthy` flag, and `strict_chain_owner`
deliberately never consults `healthy` (`cluster.rs:129-142`; `AGENTS.md:47-48`). The removal that
actually feeds ownership is `region.rs:831-844`: `nodes()` drops any peer whose `last_seen_ms` is
older than 30 s — `health.rs:52-55` calls this "the mechanism that ACTUALLY removes a dead node
from service" and says it happens "regardless of this flag". So the designed HOLD covers a frozen
gossip loop, not a bounded network partition.

**Concrete sequence (which node, which round, which registry diff).** Round r: partition forms;
both sides keep ending rounds at the 8 s deadline. Rounds r+1..r+4: H's `last_seen_ms` ages past
30 s in B's registry; `region.rs:839` drops H. B's `strict_chain_owner(chain, nodes)` now returns B
(`cluster.rs:131-142`, input from `state.rs:594-605`). B is a backup: `quorum_required` is true
(`leadership.rs:224-226`), but `voter_reach` counts B (self) + C (direct contact) = 2 of 3 →
`has_majority` true (`leadership.rs:228-230, 284-309`); `view.fresh` true, `isolated` false because
B sees C (`state.rs:522-524`); after `min_tenure` (120 s for BillingMeter, 600 s for
NodeDeathRelocate) `may_act` returns true (`leadership.rs:342-418`). Meanwhile in A, H's own entry
plus D stay fresh, so H remains `strict_chain_owner`, is the chain HEAD (quorum-exempt), has a
fresh view and is not isolated (D is directly reachable) → `may_act` true on H. Both resolve as
`OwnerSource::Chain` authorities (`cluster.rs:172-183`), so `is_control_plane_leader()` is true on
both (`state.rs:641-649`) and `admin_ingress` serves each side's mutations LOCALLY
(`main.rs:2245-2248`).

**Epoch fence assessment.** The `x-hive-cp-epoch` fence is evaluated only inside the
`x-hive-admin-forwarded` branch (`main.rs:2178-2205`); the header is attached by the forwarder
(`main.rs:2414-2417`), and `cluster.rs:12-16` describes it as a fencing token for forwarded admin
mutations. A locally-writing owner has NO forwarded header and NO epoch check, so both owners can
mutate unfenced. Consequence for the billing ledger: `spawn_billing_meter_loop` calls
`may_act(BillingMeter)` locally per node (`main.rs:4960-4966`), so W1 permits two meter writers.
`billing.rs:520-534` keeps real-money charging OFF by default (shadow mode) until
`HIVE_BILLING_METER_RESOURCES=1` AND `RATE_CARD.assert_profitable()`, so no charge lands by
default; the ledger rows themselves would still diverge — `docs/leaderless-migration.md:310`
documents `LedgerEntry.id` as a random UUID and `acc.balance_cents` as read-modify-write, and `:69`
documents `BillingStore.meters` as absent from the replicated snapshot.

**Existing code mitigations (facts).** No qualifying chain entry = HOLD
(`cluster.rs:185-197`); `Fallback` is forwarding-only, never an authority (`cluster.rs:33-37,
199-203`); wholesale adoption only from chain HEAD (`store_follower.rs:157-191`); forwarded
mutations epoch-fenced with retryable 503 (`main.rs:2186-2203`); promotion reconcile requires two
independent byte-identical peers (`store_sync.rs:756-770`); terms and tenures damp flapping
(`leadership.rs:311-340`); NodeDeathRelocate's 600 s tenure and off-by-default self-heal
(`AGENTS.md:52`). **Status stays CONDITIONAL**: no fleet partition was executed in this wave;
checker B's child2 reported a live-code partition *simulation* showing two `Chain` authorities
(dispatch `1794542735311-6888-8a42130732106f8a`), which is not a fleet witness.

### W2 — Two DISJOINT strict majorities from the SAME agreed voter set. **REFUTED**

`has_majority` is `reached * 2 > voters.len()` (`leadership.rs:228-230`). Two disjoint subsets of
one N-element set cannot both exceed N/2 (2*(floor(N/2)+1) > N); with the deployed 3 voters
(`HIVE_CP_VOTERS=fc-sanjose,fc-phoenix,fc-virginia-3`, `AGENTS.md:29-30`) two disjoint 2-of-3
majorities would need 4 distinct voters. Note two distinct facts that are NOT refuted: voter reach
is measured per-observer from `direct_contact_ids` (`leadership.rs:282-309`), so overlapping
majorities are possible; and with n=3f+1 a Narwhal 2f+1 quorum pair intersects in >=f+1 honest
validators, so two disjoint 2f+1 quorums cannot form over ONE agreed committee — but two
partitions each believing a DIFFERENT membership/roster can each form its own 2f+1
(`docs/leaderless-migration.md:788-795`: "HRW agreement is only as good as roster agreement").

### W3 — Differing per-node `HIVE_CP_OWNER_CHAIN` values. **CONDITIONAL**

`store_follower.rs:158-160` names this exact case: "with differing per-node chains a backup is the
owner only in some observers' views, so a node resolving a backup whose own chain never names
itself would adopt that backup's stale fork". `HIVE_CP_OWNER_CHAIN` is per-node environment
(`ansible/roles/hive_platform/templates/hive-node.service.j2:36`; `AGENTS.md:30` says a node
drop-in overrides the unit and to read `/proc/<MainPID>/environ`). With two different chain arrays,
two nodes can each resolve a different chain head, each as `OwnerSource::Chain`, and
`may_adopt_wholesale_from` compares against the LOCAL `owner_chain_from_env()`
(`store_follower.rs:176`, `cluster.rs:49-54`). Status CONDITIONAL: config drift is required and was
not induced in this wave.

### W4 — Conflicting authoritative GET/HEAD answers behind round-robin DNS. **VERIFIED** (read divergence, not two writers)

`main.rs:2242-2243` serves GET/HEAD locally; `store_sync.rs:3-11` states the class explicitly:
mutations forward to the leader but GET handlers serve the LOCAL store, "so under multi-A DNS a
dashboard read lands on a random node that never saw the leader's writes and returns empty or
stale data". Node-local state includes files under `persist::data_dir()`
(`persist.rs:148-153`, `audit.rs:12`, `admin.rs:17942`) and in-process `OnceLock`/`Mutex`
(`audit.rs:102`, `browser_admission.rs:1709`, `admin.rs:475`, `leadership.rs:170-173`).
`admin.rs:3010-3027` returns 404 for an unknown build unless the owner proxy answers. Mitigations
in code: owner proxy + `gossip::dispatch` arm + `store_sync::REGISTRY` + `MERGE_STORES` +
`db_replicate::fanout_all` (see `docs/dogfood-dashboard-facts.md`).

### W5 — Promotion-reconcile window. **CONDITIONAL** (documented in code)

`store_sync.rs:749-754`: "`admin_ingress` starts serving and forwarding writes to this node the
instant `is_control_plane_leader()` flips true ... no coordination with this function".
Checker B also cites `main.rs:4594-4629` (reconcile once per ownership term). Mitigations:
`store_sync.rs:756-771` requires >=2 independent byte-identical richer peer snapshots, keeps
local writes accepted during the fetch window, and merges `MERGE_STORES` row by row.

### W6 — Chain-less identity election with roster disagreement. **CONDITIONAL**

Without a chain, `cluster.rs:178-180` elects the lowest healthy `peer_id`; a health verdict is
per-observer (`health.rs:3-6`; `docs/leaderless-migration.md:788-795`). Mitigation already in
code: `leadership.rs:255-266` makes a chain-less node WITH a mesh roster HOLD with an ERROR, so
only a roster-less dev mesh uses the election (`AGENTS.md:47-48`).

### W7 — Empty `HIVE_CP_VOTERS` under a chain. **VERIFIED** (code + log)

`quorum_required` returns false when `voters.is_empty()` (`leadership.rs:224-226`), and
`announce()` logs an ERROR that says a backup "takes over leader-only jobs on gossip presence
alone, without proving a voter majority" (`leadership.rs:494-500`). It is a log-and-continue
ERROR, not a boot refusal. `CHANGELOG.md:2876-2877` calls it "an ERROR at boot (no derived
voters)". The majority gate is therefore configuration-dependent, not structural.

## 5. Where a Narwhal/Tusk quorum would sit relative to the chain (facts, no verdict)

- **Quorum arithmetic.** Over one agreed n=3f+1 committee, 2f+1 quorums intersect; two disjoint
  2f+1 quorums cannot exist. The paper's model assumes a fixed validator set (§2.1); our roster is
  churn-native with a 30 s TTL drop (`region.rs:831-844`;
  `docs/leaderless-migration.md:760-767` calls a monotonic voter set a regression). Our existing
  majority is not over a committee but over a configured *voter name list* reached by DIRECT
  contact (`leadership.rs:210-211, 282-309`), evaluated per observer.
- **Layered UNDER the strict owner chain (Narwhal as mempool).** A DAG of availability
  certificates would carry batches, causal history and 2f+1 availability attestations. It would
  not change who may write: `cluster.rs:129-142`, `leadership.rs:237-253`, `main.rs:2242-2248`,
  `store_follower.rs:175-191` are all authority checks, and none reads a DAG or a certificate.
  Facts: no second write authority is created by dissemination alone, and none of W1-W7 is closed
  by it. It would be a fourth dissemination substrate (§3).
- **Layered OVER the chain (Tusk ordering).** Tusk's authority is a per-wave, coin-elected
  validator whose commit is finalized by certificate counts (§1.3); ours is one static chain head
  (`cluster.rs:131-142`). Concrete collisions in live code: a coin-selected leader that is a chain
  backup is (a) refused wholesale adoption by `cluster.rs:49-54` / `store_follower.rs:184-191`,
  (b) blocked by `may_act` without a voter majority (`leadership.rs:355-363`), and (c) demoted to
  per-row `MERGE_STORES` (`store_follower.rs:130-132`). Vitra states "no consensus"
  (`crates/hive-contract/src/replicate.rs:3`; `crates/hive-contract/src/lib.rs:7-8`) and
  `docs/verifiable-database.md:175-180` says "No vote, quorum, majority, leader election over log
  contents, or fork resolution by agreement". Checker A: 3/3 valid refutations of "no second
  competing notion of authority" (§7).
- **Does a DAG mempool change wholesale-replace safety? No, by the code's own predicate.**
  `may_adopt_wholesale_from` is a NAME/SOURCE check (chain head or chain-less election), not a
  data-availability check (`cluster.rs:46-55`). `crates/hive-contract/src/replicate.rs:11-16,
  221-242` mirrors the same predicate for `Replica::adopt_wholesale`. A certificate of
  availability over a backup's snapshot would not make it wholesale-adoptable; the promotion
  reconcile's "2 independent byte-identical peers" rule (`store_sync.rs:756-770`) is likewise an
  evidence rule about peers, not about certificate counts.
- **Billing ledger vs two concurrent certificate chains.** ONE `may_act(BillingMeter)` writer is
  enforced per node view (`main.rs:4960-4966`); `admin.rs:661` gates the token-mint enterprise
  lock on the same gate; `RATE_CARD`/`COST_CARD`/`assert_profitable`
  (`billing.rs:322-335, 375, 509-517`) and the charging opt-in (`billing.rs:520-534`) bound the
  money path. Two certificate chains would each carry valid availability proofs for a DIFFERENT
  ledger batch; `crates/hive-contract/src/replicate.rs:20-23` refuses a shorter/different history
  as `MergeError::Regress`, and a fork is REFUSED with "the operator has to pick one"
  (`docs/verifiable-database.md:175-176`). Facts: there is no consensus to choose between the two
  chains, no bond/incentive model (`docs/smart-contracts.md:155-156`), and the existing design
  doc's B1 row (`docs/leaderless-migration.md:310`) describes making metering convergent by
  deterministic ledger ids and derived balances — a schema change, not an ordering protocol.
- **Partial synchrony / liveness vs HOLD.** Narwhal-HotStuff is partially synchronous (abstract:
  "loss of liveness can result in higher latency"); Tusk is randomized asynchronous and stays live
  "even if the adversary fully controls the network" (§5.2). Our gate deliberately HOLDS when no
  qualifying chain entry exists (`cluster.rs:185-197`, `leadership.rs:346-350`) and HOLDs every job
  fleet-wide while an owner is present but unable to act (`leadership.rs:18-22`). `may_act` reads
  only chain, registry, freshness, isolation, voter contact and tenure (`leadership.rs:342-418`);
  it consumes no ordering decision. Facts: a Tusk commit stream and a HOLD state can coexist —
  commits would accumulate with no job allowed to act — and a partially synchronous layer would
  itself lose liveness under the same asynchrony. `docs/leaderless-migration.md:751-759` records
  the WAN timing argument (15-30 s Raft timeouts vs hive's 3 s cluster loop).
- **What a certificate of availability WOULD and WOULD NOT add to our inclusion proofs.** Our
  proofs (`crates/hive-contract/src/inclusion.rs`, `audit.rs`, `appendonly.rs`, `checkpoint.rs`;
  `docs/verifiable-database.md:182-186`) already establish: a specific entry was published under a
  pinned key with its ack; the index hash follows from replay; two signed statements about one log
  length conflict; a served log withholds an attested tail; a checkpoint covers an entry. A
  Narwhal certificate would add 2f+1 attestations that >=f+1 honest validators HOLD the bytes
  (retrievability) plus a causal-history DAG link and digest integrity. It would NOT add freshness
  (`AGENTS.md:204`: "Inclusion/append-only proofs show an entry WAS published, not freshness"),
  external anchoring, deterministic ordering (Tusk's job), authorization, or a fork-resolution
  rule. The paper itself states no freshness or external-anchor claim (§1.5).

## 6. Verdict HELD (exact gate)

No verdict on Narwhal/Tusk's usefulness. The gate is the user's ordering sentence quoted at the top
of this doc: research/analysis of usefulness proceeds only after the task above is "finished and
verified", and this wave is the FACTS wave. A separate, later hop AFTER the implementation wave's
verification receipt exists writes the verdict. Nothing here weakens or proposes removing
`HIVE_CP_OWNER_CHAIN`, `Cluster::strict_chain_owner`, `leadership::may_act`, the voter-majority
requirement, or the wholesale-replace authority predicate.

**Gate refinement (2026-11-12; receipts below reported by the orchestrator, NOT independently
re-run in this facts wave).** Partial verification receipts now exist: `cargo build -p
hive-contract --offline --bin contract-billing` exit 0 (4.63 s); `./target/debug/contract-billing`
exit 0 with `RESULT PASS probes=11 replay_ok=true signed=9 proofs=7 rewritten_detected=true
omitted_detected=true chain_tamper_detected=true` (G1/G2/G3 deserialize bypass refused);
`cargo check -p hive-cloud --offline --bin hive-cloud` exit 0 (`Finished dev profile in 1.68s`),
with one earlier transient E0308 mid-edit and both runs logged. These are PARTIAL and do not
satisfy the gate: `cargo check` skips `cfg(target_os="linux")` and test targets, and a mid-edit RED
run is not a final-tree result. The remaining condition is the **FOUR-LAYER gate on a FINAL FROZEN
tree**: `cargo test --workspace --locked`, `scripts/cli-acceptance.sh`,
`check-contract-bindings.py --check`, and `git diff --check` (CI runs the layers on Linux AND macOS
per `AGENTS.md`). PRD row `narwhal-usefulness-verdict-held-sc3` stays OPEN with that named blocker;
a dedicated verdict hop is dispatched only after that receipt exists. No recommendation is issued
here.

## 7. Adversarial recursion: vote counts

Two checker children, each spawning two refuters (6 agents total, each with its own SESSION_ID and
heartbeat). A checker that errors casts no vote; two valid refutations kill a claim.

| Claim | Valid refutations | Votes cast | Errors | Status |
| --- | --- | --- | --- | --- |
| (a) "Narwhal/Tusk can be added without creating a second, competing notion of authority" | 3 | 3 | 0 | **KILLED** (2 refuters + checker's own independent assessment, all VALID REFUTATION) |
| (b) "our current control plane has no split-brain window at all" | 2 | 2 | 0 | **KILLED** (both refuters VALID REFUTATION; checker's non-voting independent assessment also found a VALID REFUTATION) |

Checker A's counterexamples: `cluster.rs:49-54, 63, 131-142, 268-274`; `leadership.rs:237-253,
342-354, 416-418`; `store_follower.rs:131-147, 157-162, 175-191`; `main.rs:4964`;
`crates/hive-contract/src/lib.rs:7-8`, `replicate.rs:3-16, 29-30, 221-242`;
`docs/verifiable-database.md:175-180`; `docs/leaderless-migration.md:10, 56, 671`. Checker B's
three windows are W1, W4 and W5 above (with `crates/hive-edge/src/region.rs:832-841`,
`state.rs:522-524, 594-605, 641-650`, `main.rs:2242-2249`, `admin.rs:3010-3030`,
`gossip.rs:1002-1018`, `store_sync.rs:750-770`). Both checkers were read-only and issued no
verdict.

## 8. Executed witnesses for this hop

- WebFetch: `https://arxiv.org/abs/2105.11827` (abstract quotes, 2026-11-12);
  `https://ar5iv.labs.arxiv.org/html/2105.11827` (section list, §2.1/§3.1/§3.2/§3.3/§5.2/§7/§8.4
  quotes, absence of "view-change"/"checkpoint"/"state-sync"); `https://arxiv.org/pdf/2105.11827`
  (binary dump, text unavailable).
- `gm.callers` `1794541826633-5848-633fc138616ebb00` (`may_act` defined at
  `leadership.rs:416-418`, 17 distinct callers incl. `main.rs:4964`); `gm.impact`
  `1794541827968-5850-445926b6d040cf56`; `gm.codeinsight outline`
  `1794541829085-5852-68b81ec29e0429c8` (`leadership.rs` 26 symbols).
- `gm.codesearch literal` (own live run): `1794541525604-5668-46510657d6da2824`
  (`HIVE_CP_OWNER_CHAIN`), `1794541534255-5670-6dd7c9532533cc36` (`strict_chain_owner`),
  `1794542223415-6288-6ff63f8890a2fbec` (`HIVE_CP_VOTERS`),
  `1794542237570-6305-a88da9666e35fcac` (`MERGE_STORES`),
  `1794542251046-6326-80168b4954e4e366` (`may_adopt_wholesale_from`),
  `1794542266964-6340-afee6771cdf9f421` (`HIVE_BILLING_METER_RESOURCES`),
  `1794542478478-6598-95195c0487463b39` (`admin_ingress`),
  `1794542700609-6855-32b6682c72c79891` (`fn admin_ingress`, exhaustive scoped),
  `1794542702348-6856-8ac2a2fa0191fd6c` (`async fn build_get`, exhaustive scoped),
  `1794543027379-7075-ebec29daaf5cffbb` (`x-hive-cp-epoch`),
  `1794543734088-7612-d858cb214feacc86` ("There is no consensus"),
  `1794543746799-7621-8562086c72b649cc` ("no global ordering"),
  `1794541152522-5576-173249d22687f571` (heartbeat write).
- Host `Read` of located paths: `cluster.rs:1-215`, `leadership.rs:1-200, 200-435, 458-511`,
  `store_follower.rs:1-205`, `store_sync.rs:1-40, 748-777`, `main.rs:2160-2249, 2408-2442,
  4955-4979`, `health.rs:1-125, 125-160, 225-334`, `gossip_round.rs:1-130, 130-174`,
  `bounded_round.rs:1-40`, `persist.rs:140-169, 240-267`, `billing.rs:315-354, 505-545`,
  `state.rs:490-529, 590-659`, `region.rs:825-852`, `gossip.rs:1-30, 398-427`,
  `docs/leaderless-migration.md:1-220, 220-460, 660-839`, `docs/verifiable-database.md:170-187`,
  `docs/smart-contracts.md:145-173`, `crates/hive-contract/src/lib.rs:1-16`,
  `crates/hive-contract/src/replicate.rs:1-60`, `ui/lib/api.ts:1-45`, `ui/README.md:55-74`.
- Checker/refuter live runs: checker A `1794543284957-7353-ef312e5d3e4d607f` (final phase-status),
  its refuters' codesearch/callers/WebFetch dispatches listed in its report; checker B
  `1794543790973-7649-7e620e22cdeff4d1` (final phase-status) and
  `1794542735311-6888-8a42130732106f8a` (live-code partition simulation).
