# Contract log explorer (admin ops)

Present tense. History lives in `CHANGELOG.md`.

## What this is

`GET /v1/admin/contract/explorer` serves a NODE-LOCAL, operator-only view of ONE contract's
append-only operation log; `ui/app/admin/explorer/` renders it. It exists so an operator can
observe recorded smart-contract operations, the executor's acknowledgement of each, and the
hash chain that links them.

This platform has **no blocks**. There is no height, no confirmations, no finality and no
consensus. The log is a sequence of entries keyed by `seq`, each committing to its predecessor.
The page is called a *contract log explorer* rather than a block explorer: naming it otherwise
would assert properties the system does not have.

## Node locality is deliberate

`admin_ingress` serves GET/HEAD LOCALLY and public hosts are round-robin DNS, so every node
answers with ITS OWN signed oplog shard. There is deliberately NO owner proxy, NO `?node=` hop
and NO `gossip::dispatch` arm: presenting one node's shard under another node's name would
misattribute the evidence, and an unknown gossip path answers an EMPTY body. The response names
`responding_node`; a verifier that wants a different node addresses that node's admin listener
directly. The first panel of the page repeats this, because a Refresh may reach a different node.

The log is a **retained window**, not the whole history: entries before `shard.base` were
compacted away under a signed checkpoint. `seq` is a per-contract sequence number in this node's
own shard; seqs from different nodes are not comparable.

## Authorization

`require_auth_read` refuses an anonymous GET with 401 on an enforced node (the dashboard's
cookie re-mint keys on 401), `roles::RULES` lists the route at ADMIN, and the handler adds
`require_operator`. `roles::gate` decides the role once and short-circuits for `platform_admin`,
so the ADMIN floor never blocks the operator. Raw entries carry tenant tags, actors and refusal
reasons: a viewer floor would leak other tenants' rows. `ui/lib/team-role.ts` mirrors the row.
The route is NOT in `auth::is_open_route`.

The page is behind `ui/proxy.ts`'s `/admin(.*)` owner allow-list and prefetches server-side with
`fetchOpsResult`, which mints an operator bearer. There is NO client poller: a browser `/ops`
hop carries only the tenant cookie, which an operator-only route correctly refuses. Refresh
re-runs the server render.

## What is measured, and what is not

Every displayed verdict is RECOMPUTED from this node's retained window on `spawn_blocking`,
never read from a stored "verified" flag:

- `chain`: `OpLog::verify_chain()` from the attested `base_prev`, so it stays valid after
  compaction. Bounded by a replay budget; when the budget is exceeded `ok` is `null` and the
  reason is reported, never silently "verified".
- per entry: `LogEntry::expected_hash()` vs the stored `entry_hash`, the `prev_hash` link against
  the preceding entry (or `base_prev` at the window's base), and `ack.log_head == entry_hash`.
- `index_replay`: `audit::verify` replaying the deterministic `exec::apply` transitions and
  comparing the recomputed index hash to the last published `ack.index_hash`. This is only
  sound from genesis; when `base != 0` the page reports that a signed checkpoint is needed to
  resume, instead of reporting a false divergence.
- payload display is bounded in keys and value length, with credential-shaped keys redacted.
  Canonical bytes are LENGTH-PREFIXED `<len>:<bytes>` and are NEVER reconstructed or shown as
  a preimage.

No per-entry signature is verified by this view: the retained in-memory window carries none;
signed segments are checked by `contract-audit` / `contract-transparency` with a PINNED key.
There is no pinned independent identity, so `pin_required` is true.

Residual (accepted, not hidden): `tenant`, `actor` and `target` are shown BOUNDED to an operator.
They are the verified server-derived tags of the recorded op, never taken from `x-hive-team`.
An operator console legitimately sees cross-tenant operational metadata (as `/v1/admin/audit`
already does); a viewer must not, which is why the route is not viewer-floor. Credential-shaped
payload KEYS are redacted and all values are length-capped, but the crate does not guarantee that
an arbitrary `Ack.error` string or an operator-authored payload value contains no sensitive text.
Only the three allowlisted contract ids are accepted; a caller-controlled string never reaches a
path, URL or HTML attribute.

What the view proves: publication, hash-chain integrity and deterministic index replay from
genesis. What it does NOT prove: freshness, consensus, finality, fraud detection, an
attestation or a compliance verdict. Honesty here is by REPLAY, not agreement; there is no
bond model. A structural self-check under the writer's own key is not fraud detection.

## Recorded outcomes, not execution guarantees

`crates/hive-cloud/src/db_contract.rs` is the only seam, and it does NOT authorize or fail the
request: the caller mutates FIRST and records afterwards. A REFUSED entry is therefore a
RECORD of a write that already happened, not a prevention. `db.write` outcomes are
MEASUREMENTS, not `Ok(())`. A REFUSED entry shows as `REFUSED (a record)`, never as "failed
transaction". A persist fault can leave NO entry at all, so an empty window is unknown, not
clean — the page states this explicitly.

## Verification runbook

No test files. From the repo root:

```bash
cargo test --workspace --locked
TMPDIR=/private/tmp scripts/cli-acceptance.sh
python3 -I scripts/check-contract-bindings.py --check
git diff --check
ui/node_modules/.bin/tsc --noEmit -p ui/tsconfig.json
(cd ui && npm run lint)
```

Live witness: run the real `hive-cloud` binary on loopback with a fresh `HIVE_DATA`, mint an
operator JWT (`/v1/token` with `x-hive-internal` and an owner email in `HIVE_OWNER_EMAIL`), then
GET `/v1/admin/contract/explorer?contract=hive.billing-journal.v1&limit=5`. Probe the anonymous
401, the non-operator 403, an unknown contract id (400), a clamped `limit`, and the recomputed
verdict fields. Adding `roles::RULES` or `TEAM_ACTIONS` rows changes pinned spans, so
`python3 -I scripts/check-contract-bindings.py --sync` is run LOCALLY (never in CI) and the
regenerated manifest is committed.

Live witness observed (2026-11-14, real binary, fresh data dir): anonymous 401, viewer 403,
operator 200, unknown contract 400, `limit=99999` clamped to 100. A real `POST /v1/databases`
`db.create` produced a managed-db entry at seq 0 with `declared_effect: StateWrite`,
`hash_ok`, `prev_link_ok` and `log_head_ok` all recomputed `true`, `chain.ok: true`,
`index_replay.available: true`, `pin_required: true`, `entry_signature.verified: false`. All
three contracts answered with the full honest envelope.

Tamper witness: a COPY of the persisted `seg-000000000000.jsonl` was rewritten with a different
`op.payload.project` while keeping the original `entry_hash` and signature. `Store::open`
rejected the segment and rebuilt from the signed checkpoint (`base=1`, `retained=0`). The
explorer reports `chain.ok: null` with "no retained entries to walk; an empty retained window is
UNKNOWN, not clean" rather than a vacuous green "consistent" — an empty window is never rendered
as a clean one. This is why a rejected/faulted log is not silently shown as verified.

## Roll

At the operator's word (`roll deploy`), `ansible/playbooks/parallel-deploy.yml --tags backend,ui
--limit 'fc-orlando:va:va3:sj:sj7:phx'` rolled this change to the six live hosts. Pre-roll
`scripts/audit-secret-keys.sh` passes 6/6. `ROLL_EXIT=0`, `failed=0 unreachable=0`,
`BACKEND VERIFIED 6/6`, `UI VERIFIED 6/6` on one BUILD_ID `BPVFN-8KAurgXrQEn8cWr`, with
`/admin/explorer` in every node's `app-paths-manifest.json`.

The RUNNING binary is proved, not the source: before the roll `/proc/<MainPID>/exe` on all six
hashed to `34277da8e83a` with none of the three explorer literals; after, all six hash to
`01b8f355cf98` and `strings -a /proc/<MainPID>/exe` finds each literal. On each node `/healthz`
is 200, an anonymous GET is 401, and an operator GET is 200 with `responding_node`,
`pin_required: true`, a recomputed chain verdict and the honest labels. Mesh reports
`visible == expected` and `isolated=false` on all six.

Residual (pre-existing, not a roll regression): fc-orlando's peer-facing `:8787` is unreachable
from other fleet nodes because its `nft` INPUT `policy drop` PUBLIC chain never allowed 8787.
The node itself answers 8787 and 8786 with 200 and its mesh is converged.
