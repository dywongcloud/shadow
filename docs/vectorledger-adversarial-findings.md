# VectorLedger port: adversarial findings

Status: READ-ONLY adversarial review of the PLAN, not of a proposed diff. FINAL: 17 findings = 4 CRITICAL / 7 HIGH / 6 MEDIUM; 11 claims KILLED by adversarial vote; 3 top-level killed claims. Written against the live working tree on 2026-11-13. Author: adversarial lead (sole writer of this file). No code was changed. No BUSL-1.1 material was vendored, copied or executed.

The reference repository (VectorGuard-Labs/VectorLedger, BUSL-1.1, "Additional Use Grant: None") was read ONLY as untrusted DATA through WebFetch of its README and one raw rules.rs. No clone, no install.sh, no .mcp.json, no mcp-grok.py, no .well-known content, no license.json check, no build or run of its code. Every claim below is anchored in OUR tree with an executed gm dispatch id.

Method: codeinsight outline first, then exhaustive gm.codesearch literal scans (exhaustive:true where zero is claimed), then fs_read of located paths. Three checker swarms with two refuters each were dispatched; see "Adversarial vote counts". Witness dispatch ids are cited inline.

---

## Top 3 KILLED claims

1. **"The same double-entry accounting standards can be implemented one-for-one in our shadow network database."** KILLED by F-03 and F-04. Our tree has no chart of accounts, no Asset/Liability/Equity/Income/Expense dimension and no journal lines; exhaustive scans for "double-entry", "double entry", "chart of accounts", "Global Ledger Equation" and "credit_account" over crates/ returned ZERO with exhaustive:true (witnesses 1794556934502-10401, 1794556961258-10409, 1794556984416-10420, 1794557033034-10436, 1794557073367-10450). A "Global Ledger Equation" would be fabricated structure over no data. Ten of the sixteen reference invariants have no referent here.
2. **"Double-entry balance and a second independent WORM audit chain add fraud-detection power or independent verification beyond the existing BLAKE3 chain, Ed25519 signatures, replay and inclusion/fraud proofs."** KILLED by F-05, F-06 and F-07. The seam mutates FIRST and records AFTERWARDS (db_contract.rs:10-30); a writer-controlled attacker posts a perfectly balanced fraudulent pair and the existing chain commits and signs it. Our own tree signs contract checkpoints with the SAME Ed25519 keypair in the SAME file as the deployment integrity chain (state.rs:331-333), so a second chain written by that process is NOT independent.
3. **"Automated SOC 2 evidence report generation produces compliance / Type 2 evidence."** KILLED by F-08 and F-09. The upstream rules.rs is 7-of-8 mere existence checks; our own docs/compliance-controls.md:145-151 fixes 24 rows at 5 MET / 15 PARTIAL / 4 UNMET with NO Type 2 row MET and every MET design-only. Code can emit control evidence; it cannot emit the examination, the period, the sample, the assertion or the CPA opinion.

---

## Findings

### F-01 CRITICAL -- adding a required field silently destroys ALL platform state

Evidence: crates/hive-cloud/src/persist.rs:242-247 `pub fn load() -> PlatformSnapshot` is `serde_json::from_str(&s).unwrap_or_default()`; a read error also returns `PlatformSnapshot::default()`. Any deserialize failure is therefore silent TOTAL state loss: every deployment, team, project, database, tombstone, API key, TOTP row and invoice disappears and the node boots as empty. crates/hive-cloud/src/billing.rs:718-727 `LedgerEntry` has ZERO `#[serde(default)]` -- all seven fields (id, tenant, ts_ms, kind, amount_cents, balance_after_cents, note) are serde-REQUIRED. crates/hive-cloud/src/billing.rs:647-677 `BillingAccount` has nine required fields (tenant, plan, status, included_cents, used_cents, balance_cents, period_start_ms, period_end_ms, updated_ms); only stripe_customer, stripe_subscription, tier_locked_plan and addon_lines are defaulted. crates/hive-cloud/src/persist.rs:23-146 lists 46 persisted fields, ALL top-level ones carrying `#[serde(default)]` -- the protection is field-by-field, not structural, and persist.rs:99-102 comments that this exact rule is load-bearing.

Consequence: adding debit_account/credit_account/account_type/currency to LedgerEntry WITHOUT `#[serde(default)]`, or retyping amount_cents, makes every existing state.json unparseable and silently empties the node. The same applies inside any nested type reachable from PlatformSnapshot (ProjectSettings, Team, Invoice, Checkout, EnterpriseSnapshot, SandboxesSnapshot, SyncedQueues, DataSnapshot, ...).

Smallest honest mitigation: ADDITIVE-ONLY, `#[serde(default)]` on EVERY new field at EVERY nesting depth; never remove, rename or retype a serde-required key (a "serde-REQUIRED key is never removed from a persisted struct in one release"); never add `deny_unknown_fields`; and BEFORE any roll, live round-trip a REAL state.json through the new binary and assert field-by-field equality of all 46 top-level fields plus a nonzero ledger count. A unit test is not evidence here -- the failure is silent, so the witness must compare a real snapshot.

### F-02 CRITICAL -- mixed-version fleet silently diverges billing during a roll

Evidence: crates/hive-cloud/src/billing.rs:1665-1669 documents that `snapshot()`'s TWO-element tuple (accounts, ledger) IS the store_sync wire shape: "a node on the previous binary decodes it as (Map, Vec), which fails on a 3-element array, so widening it would stop billing replicating to every not-yet-upgraded node for the length of a rollout." crates/hive-cloud/src/store_sync.rs:158-178 defines `SyncedStore { name: "billing" }` with `snapshot: |c| enc(&(by_tenant, ledger))` and `adopt` = `serde_json::from_slice(b).ok()?` -- an unparseable payload is silently discarded with NO error and NO log -- followed by `c.billing.load(...)`, which is WHOLESALE-REPLACE (billing.rs:1672-1679), not a per-row merge. crates/hive-cloud/src/store_sync.rs:530-537 `MERGE_STORES` contains only browser_admissions, browser_presence, projects, teams, production_deployments, totp -- billing is NOT in it. Wholesale adoption is only from an owner every node agrees on (cluster.rs:49 may_adopt_wholesale_from; store_follower.rs:143, :176).

Consequence: during a rolling upgrade, an old node cannot parse the new billing snapshot and a new node cannot parse an old one. Because adoption is wholesale and the parse failure is swallowed, nodes do not error -- they simply keep different ledgers and the next persister overwrites state.json from local in-memory data. A double-entry journal added to this tuple would make divergence fleet-wide and silent. There is no partial-roll compatibility window.

Smallest honest mitigation: do NOT widen the tuple. Put any new journal in a SEPARATE additive `#[serde(default)]` store with per-row MERGE_STORES adoption and permanent tombstones, encode with length-prefixed canonical bytes + enc_sorted(), and give the adopt path a LOUD refusal-to-adopt log instead of `.ok()?` silence. Prove the mixed-version decode with two binaries, not one.

### F-03 HIGH -- the "Global Ledger Equation" has no accounts in our tree

Evidence: reference README states "Sigma(Assets + Expenses) == Sigma(Liabilities + Equity + Income)" checked by check_financial_invariants(), over an accounts "Chart of accounts" table (id, code, name, account_type, currency, status, domain, balance) and ledger_lines (debit/credit lines). Our exhaustive scans over crates/ (2788 files, exhaustive:true) found no "chart of accounts", no "Global Ledger Equation", no "credit_account" and no "double-entry". What exists is crates/hive-cloud/src/billing.rs:647-677 BillingAccount -- a single running-balance record (included_cents, used_cents, balance_cents) -- and billing.rs:718-727 LedgerEntry, a single-entry append with kind and balance_after_cents. Exhaustive literal scan for billing_ledger in crates/ (witness 1794556810915-10357) shows 33 hits across 7 files: persist, admin, gossip, guardian, main, relational, billing -- no account dimension anywhere. Charging is OFF by default (billing.rs:526-534 requires HIVE_BILLING_METER_RESOURCES=1 AND RATE_CARD.assert_profitable()).

SCOPING NOTE (gm.git_status witness 1794558509301-10885): the scans above cover the tracked tree at scan time. During this review the sibling code writer created UNTRACKED crates/hive-contract/src/journal.rs, crates/hive-contract/src/contracts/journal.rs and docs/double-entry-design.md. Those are the proposed port itself, not pre-existing duplication, and are exactly the files F-01/F-02/F-05 target. The "no existing implementation" claim is about the tree BEFORE this work, correctly.

SCOPING NOTE (gm.git_status witness 1794558509301-10885): the scans above cover the tracked tree at scan time. During this review the sibling code writer created UNTRACKED crates/hive-contract/src/journal.rs, crates/hive-contract/src/contracts/journal.rs and docs/double-entry-design.md. Those are the proposed port itself, not pre-existing duplication, and are exactly the files F-01/F-02/F-05 target. The "no existing implementation" claim is about the tree BEFORE this work, correctly.

Consequently the port is not "implementing a standard" but INVENTING a general financial ledger and populating it with metering events. Fabricated structure with no data yields a permanently satisfied equation that proves nothing.

Smallest honest mitigation: describe the work as "metering/credits ledger hardening", not double-entry accounting. Do not add an account chart. If a credit/debit sign convention is desired, express it as a typed enum over the EXISTING kinds (charge/credit/plan_change/renewal) with a signed i64 amount, which is what billing.rs already has.

### F-04 HIGH -- 10 of the 16 reference invariants are theater in our domain

Evidence (reference README, data only): the 16 are (1) balanced totals, (2) two lines minimum, (3) no zero amounts, (4) account existence, (5) closed accounts, (6) currency match, (7) category non-negativity, (8) exposure ceiling, (9) legal holds, (10) four-eyes approval, (11) only Posted reversible, (12) AlreadyReversed, (13) gapless rising sequence, (14) BLAKE3 per-entry chain, (15) currency precision, (16) idempotency keys.

No referent here: 4, 5, 6, 7, 9, 10, 11, 12, 15 (no accounts, no account types, no currency -- all amounts are integer cents i64 --, no lifecycle, no second approver principal: there is ONE may_act(BillingMeter) writer, and no retention policy; billing.rs:1038 prune_ledger_range DELETES raw rows). Theater-by-duplication: 1, 2, 14 (crates/hive-contract already has a BLAKE3 hash-chained append-only oplog with signed checkpoint windows and Merkle roots; oplog.rs:1-9). 8 and 3 already exist as bounds: billing.rs:812 MAX_AMOUNT_CENTS with checks at :1037, :1121, :1340 preventing the `u64 as i64` negative; 13 is meaningful but ALREADY has known residuals (docs/compliance-controls.md:177-184 OC-04 hidden segment, OC-05 unvalidated timestamps, OC-06 physical reorder, OC-07 compacted-prefix blindness). Genuinely new and worth taking: 16 (idempotency keys), strictly at the store primitive, never in a handler.

Smallest honest mitigation: port invariant 16 only; re-state 13 with its residuals; explicitly mark the rest NOT APPLICABLE in the portability matrix. Do not claim "16 invariants enforced".

### F-05 CRITICAL -- a balanced but fraudulent entry is undetectable; balance adds zero writer-fraud detection power

Evidence: crates/hive-cloud/src/db_contract.rs:10-30 states verbatim "It does not authorize", "It does not perform the mutation, and it does not run BEFORE it. Every caller here mutates first and records afterwards ... A REFUSED entry is therefore a RECORD that the platform's write was not attested -- never a prevention of it. The bytes have already moved." crates/hive-cloud/src/db_contract.rs:242-292 `wrote()` calls `c.databases.record_write(&db.id, &bytes)` on `Ok` and then `record(...)`, with an explicit comment that "It counts the write even when the CONTRACT refuses it"; the None arm records an unattributed write. crates/hive-cloud/src/db_contract.rs:300 documents the same three rules for unattributed writes.

Attack: an attacker who controls the writer posts (debit X, credit X) against a fabricated account pair with a back-dated ts_ms, a false note and a chosen tenant. The arithmetic invariant is satisfied by construction; replay confirms the entry was published; the BLAKE3 chain folds it; the Ed25519 checkpoint signs it. Debits == credits is a LOCAL arithmetic invariant evaluated by the same code that mints the entry. It catches a coding bug that forgets the second leg; it catches no writer fraud. What it adds beyond our existing chain is ZERO for the adversary who matters, and it adds a whole new persisted struct that can trigger F-01.

Smallest honest mitigation: if a balance check is added anyway, describe it as a "writer self-consistency assertion", never fraud detection. Real detection remains: pinned independent signer identity, external anchoring, and replay against a second independently-owned log. See TRUST LIMITS.

### F-06 CRITICAL -- a second chain written by the same process with the same key is NOT independent

Evidence: crates/hive-cloud/src/state.rs:331-333 (witness 1794558025294-10764) on the contract oplog: "Each checkpoint is signed with this node's own IntegritySigner -- the same Ed25519 keypair, in the same file, resolved by the same fail-closed open_or_create, so there is no second key to rotate and no way to tell which identity signed an artefact." crates/hive-cloud/src/integrity_signer.rs:1-13 (witness 1794558072714-10778): ONE per-node keypair at $HIVE_DATA/integrity_signing.key, chmod 600; rotation keeps the old public key in the verifier trust set. crates/hive-contract/src/oplog.rs:1-9 (witness 1794558113919-10791): the oplog is "the CONTENT commitment, not a replacement for transport authentication". docs/compliance-controls.md:166-172: no consensus / honesty by REPLAY, NO bond model, no external anchor, no breach-notice delivery.

The reference WORM audit log is O_APPEND|O_CREAT with a BLAKE3 chain "IDENTICAL TO THE LEDGER CHAIN" (orchestrator reading of crates/vledger-audit/src/lib.rs, relayed as data). Two hash chains written by the SAME process with the SAME key and NO external anchor do not constitute independent evidence: an attacker who controls the writer rewrites both. Asking the four questions explicitly of OUR tree: separate key? NO. Separate signer? NO. Separate storage? NO (both under $HIVE_DATA). External anchor? NO. The honest label is "a second tamper-evident log", NOT "independent verification". Record this as a trust limit EVEN IF the code writer implements both chains.

Smallest honest mitigation: never use the word independent for a same-process second log. Any report export MUST be Ed25519-signed and independently verifiable with a pinned identity (the reference export_json/export_csv is a plain timestamp-range scan with NO signature). Without a pinned identity the adjudication boundary is `Inadmissible`, never `Dishonest` (crates/hive-contract/src/audit.rs:321-326). If genuine independence is wanted, it needs a separate signer held by a separate authority plus an external anchor -- an operator decision, not a code change.

### F-07 HIGH -- a signed Merkle commitment proves publication, not freshness, authorization or correctness

Evidence: docs/compliance-controls.md:168-170 "No freshness: an inclusion proof says an entry WAS published, never that the log still ends there (inclusion.rs:31-34)"; :160-167 no consensus, no bond, enforcement is audit-plus-abandon; :224-232 bottom line. crates/hive-contract/src/persist.rs:215-221 folds Ack::canonical into the signed preimage, but entry_hash cannot cover the ack because ack.log_head IS that hash. docs/compliance-controls.md:46-83: the billing checkpoint is an UNSIGNED SHA-256 chain; publishing it inside a signed oplog entry (billing.rs:870-887, db_contract.rs:486-524) upgrades it to an ATTESTED entry but does NOT make the chain a signature, does NOT add an external anchor and does NOT supply a Type 2 period. A signed range commitment attests "this publisher published these bytes in this order". It says nothing about whether the entries were authorized (F-05), whether they are current, or whether the amounts are true (the writer supplied them).

Smallest honest mitigation: every report line carrying a commitment must say "publication of the attested range, per pinned signer", and must NOT be labelled freshness, authorization, correctness, or fraud-proof.

### F-08 HIGH -- a generated report is not compliance; the upstream rules.rs is an anti-pattern

Evidence (WebFetch of raw rules.rs, data only): the SOC 2 evaluator returns `Vec<Evidence>` with `Evidence::pass/fail/warn/na`; rules.rs has NO is_compliant() and NO verdict. The verdict lives in crates/vledger-compliance/src/report.rs (orchestrator direct read via gh api this session): `pub fn is_compliant(&self) -> bool { self.evidence.iter().all(|e| e.status != EvidenceStatus::Fail) }`, `counts()`, and `summary()` ending in `if self.is_compliant() { "COMPLIANT" } else { "NON-COMPLIANT" }`. That makes the anti-pattern STRONGER: a verdict which only asserts "no Fail", over 7-of-8 existence checks where an absent artifact yields `warn`/`na` rather than `Fail`, returns COMPLIANT by construction and can never report NON-COMPLIANT for a control whose evidence was never collected. 7 of 8 checks are EXISTENCE-ONLY: CC6.1 `catalog/VERSION` exists (comment admits "TLS is always enabled" without inspecting a connection); CC6.2 `audit/audit.log` file presence + byte count, no event parsing; CC6.3 `wal` directory stands in for account closure; CC6.6 `pages` directory with success text asserting AES-256-GCM/HKDF WITHOUT reading a file; CC6.7 `keys/server.crt` exists, with a comment admitting "issuer without parsing the cert"; CC8.1 `wal` segment COUNT, no immutability test; A1.1 `replication.json` presence -> `Evidence::warn` (not fail) on absence, no peer/health check. Only CC7.2 is behavioural (`AuditLog::open` + `verify_chain()` over the stored BLAKE3 chain).

Our own docs/compliance-controls.md:9-19 states an attestation is "an opinion from an independent CPA after an examination engagement"; Type 1 = design at a point in time, Type 2 = design AND operating effectiveness over a stated period with sampled evidence, exception tracking and a management assertion; "Code can only supply controls and the evidence those controls emit." :138-151 AT-1/AT-2 both UNMET. The reference README itself says of its reports "It does not by itself make an organization compliant."

What a CODE-ONLY report CAN genuinely evidence: CC7.2 monitoring artifacts (the live witness binaries, docs/contract-verification.md:20-31), CC8.1 change-management technical gates (.github/workflows/rust-ci.yml:65, :68, :98, :323), CC6.x logical-access artifacts (auth.rs:518 require_auth, roles.rs:325 gate, secrets.rs:103 try_decrypt, scripts/audit-secret-keys.sh:40-45). It CANNOT evidence CC1-CC5 (board oversight, HR policy, risk register with named owners, periodic monitoring runs, sampled control operation), the Type 2 operating period, or AT-1/AT-2 (management assertion, CPA opinion). A held control is not a passed control (docs/compliance-controls.md:29-34); charging OFF (:21-28) means a billing control has not operated.

Smallest honest mitigation: name the generator "control evidence exporter", have it emit per-control artifacts with dispatch ids, path:line and a stated as-of timestamp, and print NO verdict word. Preserve the 5/15/4 matrix; never compute a percentage or a "COMPLIANT" rollup from it. Existence checks, if any, must be labelled "artifact present, control operation NOT evidenced".

### F-09 MEDIUM -- the existing matrix is at risk of being overstated by a new report

Evidence: docs/compliance-controls.md:145-151 "Counts (24 rows): 5 MET / 15 PARTIAL / 4 UNMET", MET = S1-1, S1-3, S1-4, S1-8, C1; UNMET = S1-10, CC9, AT-1, AT-2; "Every MET row is MET for DESIGN ONLY; its T2 column is PARTIAL or UNMET. No row is MET for Type 2 operating effectiveness." :121-126 CC4 PARTIAL because only FOUR of eight witness binaries emit RESULT: and contract-cost exits 0 even on RESULT FAIL. :195-203 Lens B already KILLED "no metering or billing event can be dropped, duplicated, reordered or back-dated without replay detecting it" and "the billing ledger is tied to the signed contract oplog".

A new report that aggregates rows into a headline score, or that repackages CC4's PARTIAL as MET because a generator now prints a line, would contradict a document whose every row carries a dispatch id. Smallest honest mitigation: the report links to the matrix, does not restate its statuses, and adds a "no claim of compliance / no certification / no Type 2" header.

### F-10 HIGH -- CI and build blast radius; cargo check cannot witness this change

Evidence: .github/workflows/rust-ci.yml:57-68 (witness 1794557544270-10611) -- "Whitespace (hard gate)" git diff --check; "Contract bindings (hard gate)" `python3 -I scripts/check-contract-bindings.py --check` at :65; "Bindings manifest is committed (hard gate)" `git diff --exit-code -- scripts/contract-bindings.json` at :68, with the comment that a regenerated manifest must ship WITH the construct change in the SAME commit and "Blind --sync laundering ... fails here". :82-98 matrix [ubuntu-latest, macos-15, macos-15-intel], `cargo build --workspace --locked --verbose`, `cargo test --workspace --locked --verbose`; :109+ live Rust runtime HTTP witness on all three OSes; scripts/cli-acceptance.sh at :323. scripts/check-contract-bindings.py:714 emits `RESULT PASS bindings={seen_bindings} layers=4 gates={gates}` (witness 1794557800002-10698). Nine witness binaries are documented at docs/contract-verification.md:12-13 and :20-31 (witness 1794557762566-10676) with only FIVE printing a RESULT line, three emitting none, and contract-cost/contract-billing using a colon-less "RESULT PASS/FAIL". AGENTS.md process rules: NO test files ever (no test/, __tests__/, spec/, #[cfg(test)], mocking libraries); four-layer gate on Linux AND macOS; cargo check SKIPS cfg(target_os="linux") AND test targets, so a behaviour change needs a RUN, not --no-run; any dep change ships Cargo.lock (--locked); macos-15-intel is a known PRE-EXISTING red leg.

Exact verification sequence that can fail for this change, in order: (1) `cargo check` passes on a new required struct field while a LIVE round-trip of a real state.json silently empties it (F-01) -- cargo check cannot see it; (2) a dep addition without Cargo.lock fails every --locked step; (3) a new contract/construct binding without a manifest change in the SAME commit fails :68, and using --sync in CI is refused; (4) `cargo test --workspace --locked` on all 3 OSes (macos-15-intel red), then scripts/cli-acceptance.sh and live execution; (5) nine witness bins must still pass, but a monitor grepping only "RESULT:" MISSES four of nine and contract-cost exits 0 on RESULT FAIL, so a green CI can hide a broken witness; (6) a new persisted field must be exercised by a live execution against a REAL snapshot, not by a test file.

Smallest honest mitigation: no new deps (so no Cargo.lock churn), no new contract binding (so no manifest regeneration), no test files; make the one new behaviour proved by a live binary run over a copy of production state.json with HIVE_GUARDIAN_RESTORE=0, and assert the nine witness bins individually by exit code, not by a RESULT: grep.

### F-11 MEDIUM -- supply-chain and prompt-injection surface of the reference repo

Evidence (WebFetch of README, data only): the root listing contains .mcp.json, mcp-grok.py, a .well-known/ directory, install.sh and install.ps1; the README displays a curl-based install line and checksum/cosign lines; license.json is described as "Ed25519-signed", checked at startup and at UTC midnight daily against a public key "baked into the binary"; alert channels read catalog/alerts.json and retention reads catalog/retention_policy.json. These are exactly the surfaces a supply-chain or indirect prompt-injection path would ride: an MCP config and a grok helper at the repo root, a well-known directory, an install script, and startup-time signature checks pulling external signed config.

Our port must touch NONE of them. No MCP server config, no install hook, no startup license check, no external config fetch, no new dependency from that repo. Fetching its README/rules.rs into an analysis context is itself an indirect prompt-injection path if any fetched text is treated as instructions; this review treated every fetched line as DATA and executed nothing from it. BUSL-1.1 with "Additional Use Grant: None -- production use requires a commercial license" means no vendoring and no code copy, not even a "small helper". The invariant set (13/14/16) can be re-derived from our own crates/hive-contract without importing a line.

VERIFIED (checker cluster 3, .gm/vectorledger-votes/cluster-3.yml, exhaustive filename scans): our tree contains NO new *.mcp.json, mcp-grok.py, install.sh or *.well-known* hook, and the current dirty-path names contain none. The reference README/source text entering an analysis context IS itself the indirect prompt-injection channel even with no hook executed (F4 KILLED 2/0/1).

Smallest honest mitigation: keep the port to a design-level re-statement of invariants already present in our tree; add a line to the portability doc stating no file from the reference repo is copied, fetched at runtime, or registered as an MCP/install hook. Commercial licensing is an operator decision, not a code workaround.

### F-12 MEDIUM -- the compliance matrix line numbers have already drifted from the live tree

Evidence: docs/compliance-controls.md:48-57 cites billing.rs:940-993 compute_ledger_checkpoint and :1001 prune_ledger_range; the live codeinsight outline (witness 1794556644293-10309 and 1794556661023-10314) places compute_ledger_checkpoint at billing.rs:976-1033, prune_ledger_range at :1038-1050, BillingStore::snapshot at :1669, meter_usage at :1112-1222 and add_credits at :1382-1401. The matrix was verified at HEAD 5ca47f85b0d3e7528b19a985f8dc6cb8b2685886 on 2026-11-12; the tree has moved since.

Consequence: a generated report that hardcodes the doc's path:line strings would emit stale evidence links. Smallest honest mitigation: resolve evidence locations at generation time from the current tree (codesearch/callers), never from a stored literal; keep the dispatch ids as the stable witness identity.


### F-13 MEDIUM -- CORRECTION: a journal primitive DOES already exist (checker C4 refutation)

Checker cluster 1+2 refuted the broad claim "no journal implementation exists" with crates/hive-cloud/src/runtime_artifact_transfer_store.rs:91, :93, :96, :101, :103, :107, :178, :231 (exhaustive literal witness 1794559719740-11198; fs_read 1794559687955-11190). There IS a durable, bounded, contiguous non-empty-prefix "transfer chunk journal" with `MAX_CHUNKS`, offset validation and declared-package-size checks. It is a TRANSFER primitive, not a double-entry accounting journal, so F-03's narrower claim (no chart of accounts / no Global Ledger Equation / no credit_account) stands, but the broad "no journal" wording does not.

Smallest honest mitigation: do not introduce a second generic "journal" type name; reuse or explicitly distinguish the existing oplog / transfer-chunk-journal primitives. Scope any duplication claim to the TRACKED tree BEFORE the sibling code writer's untracked files.

### F-14 HIGH -- F-02 is not hypothetical: the billing store already fails to replicate in the live fleet

Two REAL open PRD rows (gm.prd_list witnesses 1794559169405-11076 and 1794559355712-11094) corroborate F-02 with observed production behaviour:

- `billing-store-follower-sync-failing-bounds-both-hit` (pending): "store follower-sync: snapshot fetch from the leader FAILED ... store=\"billing\"" with 240 s timeouts; fc-virginia /v1/billing/ledger answers in 4.503 s with BOTH HIVE_BILLING_SQL_READ_MS and HIVE_BILLING_PROXY_READ_MS exhausted, though the payload stays correct. Reported by the guardian rollout agent 2026-09-30, predating recent deploys.
- `billing-invoice-history-authority-local` (pending): "store_sync.rs REGISTRY 'billing' entry snapshots only (accounts, ledger)"; finalized invoices are never replicated and a billing-authority failover loses invoice history in-process.

Consequence: the billing snapshot is ALREADY too large/unreliable to reach followers across the fleet. Widening that tuple with journal lines, or adding a second chain of entries into it, makes an EXISTING 240 s failure permanent and enlarges the wholesale-replace payload. Any design that puts journal entries on the billing replication path ships into a known-broken lane.

Smallest honest mitigation: keep the journal OUT of the billing REGISTRY entry entirely; if it must replicate, use a separate small per-row MERGE_STORES store with tombstones. Do not treat "the payload stays correct" as evidence the store is healthy.


### F-15 HIGH -- the proposed chart of accounts is fabricated and the Global Ledger Equation is vacuously true

Checker 1+2 refuted C3 with a direct read of the code writer's untracked crates/hive-contract/src/journal.rs (verified here, fs_read 1794560256968-11321 and 1794560291035-11326): `default_chart()` at :761-775 inserts SEVEN hardcoded account codes (cash, accounts_receivable, prepaid_credits, equity, revenue, usage_expense, suspense) into a `Journal` whose `accounts`, `entries`, `seq_index`, `idempotency`, `balances`, `reversed`, `refusals`, `reconciliations`, `retention_events`, `worm` and `next_seq` are ALL `#[serde(default)]` EMPTY maps/vectors (:726-751). `global_ledger_equation()` at :1210-1231 sums `*self.balances.get(code).unwrap_or(&0)` over those accounts, so with no postings every term is zero and `left == right == 0` passes trivially. The equation is satisfied by the ABSENCE of data, not by any property of the platform.

This is the concrete realisation of F-03: the "standards" are being backfilled with invented accounts. It also creates a new F-01 surface: a `Journal` with eleven serde-defaulted collections is a large new persisted structure whose adoption path must be proved, and `worm: WormChain` is a second chain in the same process (F-06).

Smallest honest mitigation: do not ship `default_chart()`. If an account taxonomy is kept at all, label it a PLATFORM-OWNED CLASSIFICATION of existing metering kinds, never a chart of accounts with balances; require at least one real posting before the equation is evaluated, and state that a satisfied equation proves only that the writer posted balanced numbers.

### F-16 MEDIUM -- one mitigation is already correctly present in the code writer's tree

Adversarial honesty requires recording what is NOT broken. crates/hive-contract/src/evidence.rs:562-575 (fs_read 1794560468121-11351) REFUSES a report whose `boundary`, `overall_status` or any control `status` contains "COMPLIANT" ("a compliance verdict word was emitted"), requires the scope statement to say "Fleet-wide coverage is NOT claimed", and checks `m.rows != 24 || m.met != 5 || m.partial != 15 || m.unmet != 4 || m.type2_met != 0`. crates/hive-contract/src/bin/contract-double-entry.rs:802 is reported to carry the same guard. This is the correct shape for F-08/F-09 and must NOT be weakened.

Residual: a guard that rejects a WORD does not reject an overstated SUBSTANCE. It does not check that every emitted control artifact has a live dispatch id, that a control which HELD or is OFF is labelled as not-operating, or that retention/pruning (F-07, S1-10) is disclosed. Keep the guard and add those three checks.

### F-17 MEDIUM -- executed bindings gate confirms the CI blast-radius numbers

gm.exec_js witness 1794560529484-11362 ran `python3 -I scripts/check-contract-bindings.py --check` on the live tree: exit_code 0, `RESULT PASS bindings=34 layers=4 gates=36 fail_open=4 secrets_clean=1 manifest=scripts/contract-bindings.json`. Checker 4+5 independently measured the same at dispatch 1794557580088-10622. So 34 bindings / 36 gates / 4 layers are VERIFIED, not assumed. A new contract or construct binding without `scripts/contract-bindings.json` regenerated IN THE SAME COMMIT fails .github/workflows/rust-ci.yml:68; using `--sync` in CI is refused by that gate. Checker 4+5 also observed `gm.ci_status` (1794557706056-10658) reports CI "unknown, never success" because HEAD is unpushed -- so a green local gate is not evidence CI passed.

---

## TRUST LIMITS / NOT CLAIMED

This section is deliberately separate. None of the following is a defect to be "fixed" by this port; each is a boundary that any report MUST print.

- NOT CLAIMED: SOC 1 / SOC 2 certification, "compliance", an auditor-recognised readiness state, a Type 1 design opinion or a Type 2 operating-effectiveness opinion. No management assertion, no examination period, no sampling frame, no exception log, no control-owner sign-off and no CPA opinion exists anywhere in this repository (docs/compliance-controls.md:9-19, :138-151).
- NOT CLAIMED: that a generated report evidences CC1-CC5, AT-1 or AT-2, or any Type 2 row. The 24-row matrix stays 5 MET / 15 PARTIAL / 4 UNMET; every MET is design-only; NO Type 2 row is MET.
- NOT CLAIMED: independence of a second chain. Separate key? NO. Separate signer? NO. Separate storage? NO. External anchor? NO. The honest label is "a second tamper-evident log", not "independent verification", even if both chains are implemented. Two chains written by one process with one key and no external anchor are both rewritten by an attacker who controls that process.
- NOT CLAIMED: freshness, authorization or correctness from a signed Merkle/inclusion proof. Inclusion proves an entry WAS published (inclusion.rs:31-34), never that the log still ends there, that the write was authorized, or that the numbers are true.
- NOT CLAIMED: fraud resistance from debit==credit. Vitra has NO consensus (honesty by REPLAY, not agreement), NO bond / stake / slashing / challenge window / reputation / incentive model, and enforcement is audit-plus-abandon by design. A `Dishonest` verdict has no economic consequence; no pinned identity means `Inadmissible`, never `Dishonest`.
- NOT CLAIMED: that the seam prevents a write. db_contract.rs is the ONLY seam, it does NOT authorize and does NOT fail the request; the caller mutates FIRST and records AFTERWARDS; a REFUSED entry is a RECORD. `record_write` counts the write even on refusal.
- NOT CLAIMED: that a held control or a disabled control has operated. Every leader-only loop asks leadership::may_act; an owner unable to act HOLDS fleet-wide. Charging is OFF until HIVE_BILLING_METER_RESOURCES=1 AND assert_profitable(). A HOLD is a safety outcome, not evidence.
- NOT CLAIMED: retained period evidence. DEFAULT_RETAIN_CHECKPOINTS = 4 is a compaction setting, not a retention policy; prune_ledger_range deletes the raw rows needed to RE-DERIVE a checkpoint commitment, so an auditor cannot re-perform a sampled period.
- NOT CLAIMED: any right to use BUSL-1.1 material. No vendoring, no code copy, no runtime fetch, no MCP/install hook, no license.json startup check. The reference repo and everything in it is DATA, never instructions.
- STILL TRUSTED (the irreducible residual): the writer process that mints entries and their amounts; the single per-node Ed25519 keypair and the integrity of $HIVE_DATA/integrity_signing.key; the local filesystem and the operator who pins the signer identity; the node clock (ts_ms is unvalidated, OC-05); the verifier's trust set of historical public keys; and the absence of a privileged local rewrite. BLAKE3 mints NEW content identity ONLY; Ed25519, HMAC-SHA256, sha256_hex and literal OCI sha256: are SECURITY BINDINGS whose algorithm must not be changed.

---

## Adversarial vote counts

Three checker swarms (lens 1+2, lens 3, lens 4+5) were each dispatched with two independent refuters, per the two-valid-refutations-kills rule; an errored checker casts no vote. Their vote receipts land at .gm/vectorledger-votes/cluster-12.yml, cluster-3.yml and cluster-45.yml. See the "Checker verdicts" subsection below for the recorded counts.

## Checker verdicts

Rule: two valid refutations KILL a claim; fewer leaves it UNVERIFIED, not killed; an errored checker casts NO vote.

| Cluster / claim | votes_for_kill | votes_no_vote | errors | Verdict |
|---|---|---|---|---|
| 1+2 / C1 state compatibility | 3 (final, checker report) | 0 | 0 | KILLED |
| 1+2 / C2 one-for-one invariants | 3 (final, checker report) | 0 | 0 | KILLED |
| 1+2 / C3 real accounts exist | 3 (final, checker report) | 0 | 0 | KILLED |
| 1+2 / C4 no existing journal | 3 (final, checker report) | 0 | 0 | KILLED (see F-13: a TRANSFER chunk journal exists) |
| 4+5 / S1 report is compliance | 2 | 0 | 1 | KILLED |
| 4+5 / S2 is_compliant anti-pattern | 2 | 0 | 1 | KILLED |
| 4+5 / S3 cargo check sufficient | 2 | 0 | 1 | KILLED |
| 3 / F1 balance adds fraud detection | 3 | 0 | 0 | KILLED |
| 3 / F2 signed Merkle = freshness/auth/correctness | 3 | 0 | 0 | KILLED |
| 3 / F3 WORM chain independent | 3 | 0 | 0 | KILLED |
| 3 / F4 no indirect prompt-injection path | 2 | 0 | 1 | KILLED (errored refuter cast no vote) |

FINAL TOTALS (all three swarms reported, on-disk files re-read at 1794560658061-11382 / 1794560306602-11328 / 1794560389927-11339): 11 claims KILLED by vote, 0 SURVIVES, 0 UNVERIFIED. Cluster 1+2: C1-C4 each 3/0/0. Cluster 3: F1-F3 each 3/0/0, F4 2/0/1 (one errored refuter cast NO vote; 2 valid refutations still kill). Cluster 4+5: S1-S3 each 3/0/0. Aggregate 32 kill votes, 0 no-votes, 1 errored checker. An earlier PRELIMINARY read (1794560070384-11276) showed cluster 1+2 at 1/2/0 and cluster 4+5 at 2/0/1; both files were rewritten to their final tallies before this line was written. The ON-DISK final files are the record of truth.

Checker 1+2 also contributed a NEW concrete refutation of C3 that this lead had not seen: the code writer's untracked crates/hive-contract/src/journal.rs:761-775 "fabricates a default chart with zero balances" -- i.e. the port DOES invent a chart of accounts with no data, exactly as F-03 predicted. Witness dispatch ids in .gm/vectorledger-votes/cluster-12.yml.

## Successor hop nomination

Nominated from the REAL pending list (gm.prd_list witnesses 1794559062394-11050, 1794559169405-11076): `billing-store-follower-sync-failing-bounds-both-hit`.

next_choice.why (verbatim, pass to the successor): "The billing store is not replicating to followers (240s snapshot fetch failures), so /v1/billing/ledger on fc-virginia now takes 4.5s - both HIVE_BILLING_SQL_READ_MS and HIVE_BILLING_PROXY_READ_MS exhausted - though the payload stays correct"

Reason: F-02 and F-14 prove the proposed journal must NOT land on the billing REGISTRY tuple, and this open row is the live-fleet evidence that the lane is already broken. The successor hop should verify, by live execution against the running binary, that the new journal store is a separate per-row MERGE_STORES store and that the billing snapshot byte size did not grow. Headroom was STOP at the time of nomination (coordinator: 12 agents live), so the spawn was handed to the orchestrator rather than opened by this lead.


## Residuals

- This review did not execute a live state.json round-trip (read-only on code, no binary run): F-01/F-02 are proved from source and from the tree's own doc comments, but the round-trip witness belongs to the code writer before any roll.
- RESOLVED (orchestrator direct read of crates/vledger-compliance/src/report.rs): the is_compliant()/"COMPLIANT" verdict exists there, not in rules.rs; see F-08 for the exact cite. This lead made no further reference fetches.
- No PRD rows were resolved by this lead; nine-stage receipts are therefore not written by this session. The doc is the deliverable.
- Checker vote files are written by the checkers, not by this lead; this file is the sole write of the adversarial lead.
- The successor hop is NOMINATED but NOT spawned by this lead: the orchestrator's "LAND ARTIFACTS NOW / headroom STOP, 12 agents already live" course correction forbade further children. The nomination and next_choice.why verbatim were handed to main in the same turn. All seven descendants reported and deleted their heartbeats.
- No push, no roll, no commit, no test file, no code edit, no branch/worktree, no BUSL-1.1 vendoring, no reference-repo file executed or cloned. Read-only on code.