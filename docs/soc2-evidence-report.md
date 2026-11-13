# Automated SOC 2 technical evidence report

This document describes the GENERATED artefact, not an opinion. The generator is
`crates/hive-contract/src/evidence.rs`; the independent verifier is
`evidence::verify_report`. The live witness is
`crates/hive-contract/src/bin/contract-double-entry.rs` (`--report <path>` writes the JSON).

## Boundary (printed on every report)

`TECHNICAL EVIDENCE SUPPORTING AN ASSESSMENT. NOT AN ATTESTATION, NOT A
CERTIFICATION, and NOT A COMPLIANCE CLAIM.` An independent CPA examination over an
operating period is required before any SOC 2 Type 1 or Type 2 attestation could be
considered. AT-1 and AT-2 in `docs/compliance-controls.md` remain UNMET: no CPA
opinion, no control-owner sign-off and no period examination exist in this
repository. No verdict word is emitted, no percentage is computed and no rollup
score is derived. The existing 24-row control matrix (5 MET / 15 PARTIAL / 4 UNMET,
ZERO Type 2 rows MET) is referenced verbatim and never recomputed or improved.

Type 1 covers the DESIGN of controls at a point in time; Type 2 covers design AND
operating effectiveness over a stated period. This report carries both fields per
control. A PASS on a design statement is NOT a Type 2 result.

## Scope

`scope: "node-local / not fleet-wide"`. The contract oplog is a node-local file under
`$HIVE_DATA/contract-oplog`, sharded by contract id; `admin_ingress` serves GET/HEAD
locally. The report names the node, the shard (`hive.billing-journal.v1`, a seq space
separate from `hive.billing-meter.v1` and `hive.managed-db.v1`) and that shard's
signed chain tip. A report generated on the strict chain HEAD is still only THAT
NODE's log. Fleet-wide coverage would need an owner proxy, a `gossip::dispatch` arm
or `store_sync::REGISTRY`. There is no consensus here: Vitra honesty is by REPLAY,
not agreement. An inclusion proof shows an entry WAS published, never freshness.

## What is machine-derived

Everything below is derived from LIVE journal state at generation time and FILTERED
by the report's OWN `[period_start_ms, period_end_ms]` range. There are no
file-existence probes and no pre-written prose standing in for evidence.

| Field | Derivation |
| --- | --- |
| `entries` | journal entries whose `posted_ms` is IN the period, with the full canonical fields |
| `commitment` | Ed25519-signed Merkle commitment over those entries' chain hashes, REUSING `merkle_root` / `merkle_prove` / `SignedCheckpoint::sign` |
| `worm_events`, `worm_tip` | the FULL append-only WORM event list (so the tip is reproducible) and its in-period count |
| `reconciliations` | immutable `ReconciliationRecord`s in the period, each with run id, entries scanned, discrepancy count and both tips |
| `invariant_violations` | typed refusals in the period, feeding the exception log |
| `retention_events` | retention-evaluation events in the period |
| `type2.exception_log` | refusals plus any reconciliation with discrepancies |
| `controls[].artifacts` | `(id, hash, ts, source_dispatch_id)` per control, one artifact per live entry/event |
| `report_hash`, `report_signature_hex` | BLAKE3 over length-prefixed canonical fields, signed by the node's existing Ed25519 identity |

Control mapping: CC1-CC9 plus A1, C1 and PI1 (12 controls). Each row carries `status`
(PASS / WARN / FAIL / N-A), `type1_design`, `type2_evidence`, a `measurement`,
`findings` and artifacts. CC6 is deliberately WARN: tenant binding is decided by
`contract::validate` upstream and this report carries no identity or role claim.

The 16 financial invariants are DOMAIN-ADAPTED, NOT the same standard one-for-one.
This platform had no chart of accounts, legal-hold system, four-eyes workflow or
multi-currency ledger. The accounts, limits and approval gates are new
platform-owned adaptations. Balanced Dr/Cr is a WRITER SELF-CONSISTENCY ASSERTION,
not fraud detection: a writer-controlled attacker can post a perfectly balanced
fraudulent pair and the chain will sign it. The WORM chain is a SECOND tamper-evident
log written by the SAME process with the SAME node identity, NOT independent
verification. Real detection needs a pinned independent signer, an external anchor,
or replay against an independently-owned log.

## Independent verifier

`verify_report(report, trusted_public_key_hex)` consults ONLY the report. It:

1. checks the exact boundary phrases and rejects any compliance verdict word;
2. checks `scope` is node-local and the fleet-wide disclaimer is present;
3. checks the 24-row matrix reference is UNCHANGED and AT-1/AT-2 stay UNMET;
4. checks `cpa_opinion == "ABSENT"`, `control_owner_signoff == "ABSENT"` and
   `operating_period_examination == "NOT PERFORMED"`;
5. checks `entries_in_period == entries.len()` and EVERY entry's `posted_ms` is
   inside the stated range;
6. re-derives each entry's `content_hash`, `chain_hash`, chain continuity, >=2
   lines and Dr/Cr balance from the carried bytes;
7. re-derives the Merkle root from `commitment.entry_hashes` and verifies the
   Ed25519 signature and an inclusion path under a PINNED key (an unpinned
   identity is Inadmissible, never Dishonest);
8. re-derives the WORM chain tip from the carried events, every reconciliation
   record hash, the report hash and the report signature;
9. checks every control has a non-empty measurement and at least one `(id, hash, ts)`
   artifact.

Reproduction of a commitment is evidence of publication, never freshness.

## Type 1 vs Type 2 honesty

`type2.entries_in_period` is a COUNT of live entries in the stated range, with the
retained sample frame named. It does NOT say a control operated effectively. There
is no sample-frame selection by an independent auditor, no management assertion, no
control-owner sign-off and no CPA opinion. An empty period is reported as a WARN
with an exception; report generation over a range with NO entries is a typed
`JournalError` refusal, never a fabricated commitment.

RESIDUAL: `billing.rs::prune_ledger_range` is called from `main.rs` right after a
finalized `LedgerCheckpoint`, and `main.rs` also retains raw ledger rows by
timestamp. Journal lines are WORM-attested before the durable billing cursor
advances, so the retained sample frame is reconstructible from the signed oplog.
A raw row pruned BEFORE its signed oplog entry lands (a refused or persist-failed
attestation) has no reconstructed sample. `crates/hive-cloud/src/audit.rs` has no
hash chain at all (detection by reopen only); that file is outside this change's
write surface and remains a named residual.

## Live witness

```
cargo build -p hive-contract --offline --bin contract-double-entry
./target/debug/contract-double-entry \
  --state /private/tmp/<copy-of-real-state.json> \
  --report /private/tmp/soc2-evidence.json
```

`--state` is OPTIONAL (informational SKIP when absent, matching the other nine
witness bins); when a path IS supplied, an unreadable, unparsable or lossy file is a
hard FAIL. The binary prints one `RESULT PASS`/`RESULT FAIL` line and exits 0/1.
The report is a technical-evidence artefact for an assessor to read, not a
deliverable that makes any organization compliant.
