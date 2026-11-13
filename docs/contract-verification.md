# Contract verification: binaries, on-disk format, and the runbook

Status: **implemented.** Ten binaries under `crates/hive-contract/src/bin/` exercise the real
`Engine`, real `Store`, real Ed25519 signing and real adjudication against generated keys and a
real filesystem. They are the verification surface for `docs/verifiable-database.md`. Present
tense; every claim cites `path:line`. There are NO `[[bin]]` sections in
`crates/hive-contract/Cargo.toml` - Cargo auto-discovers `src/bin/*.rs` - and NO clap dependency;
argument parsing is a hand-rolled `fn arg(name, default) -> String` that scans `std::env::args()`
for `--<name>` and takes the next token. Unknown flags are ignored.

**Correction to a common shorthand:** `--state <real state.json>` is accepted by SIX of the
ten binaries (`contract-billing` reports only the state file's size; `contract-double-entry`
uses a real full `PlatformSnapshot` for its S1 serde round-trip hard gate), and is OPTIONAL in
all six. The phrase "`contract-{audit,reconcile,replicate}`
--state <real state.json>" describes a real route but is not the whole surface, and the other four
binaries do not accept `--state` at all.

---

## 1. The ten binaries

| bin | parsed args | `--state` | reads | final result line | exit |
|---|---|---|---|---|---|
| `contract-adversary` | none (`bin/contract-adversary.rs:18` documents a `--dir` that is not parsed; path is fixed to `/private/tmp/hive-contract-adversary-<pid>`, `:61-66`) | no | real `Engine::open` (`:93`), generated node+attacker keys, `read_latest_checkpoint`, `read_signed_entries_from`, real `adjudicate` | `RESULT: ALL PROBES PASSED` (`:526`) or `RESULT: BREACH -- the fraud proof convicts an honest executor` (`:529`) | 0 / 1 (`:527`, `:530`) |
| `contract-audit` | `--state`, `--rounds` (20) (`bin/contract-audit.rs:82-83`) | YES, optional, default `/tmp/vitra-real-state.json` | `Snapshot { databases }` with id/project/team/kind/region only; `connection` deliberately absent (`:9`, `:31-45`) | `ALL PROBES PASSED` (`:858`) or `{n} PROBE(S) FAILED` (`:860`); **no `RESULT:` line** | 2 unreadable, 3 no databases, 1 probe failure, 0 pass |
| `contract-cost` | `--dir`, `--ops` (1000), `--window` (256) (`bin/contract-cost.rs:133-135`) | no | real `Engine`/`Store` on `dir/log` and `dir/log-deferred`; `signing.key` 0600 | `COST {label} mean_us=... p50_us=... p95_us=... p99_us=...` (`:67-72`); `RESULT PASS` / `RESULT FAIL` (**:222-224, no colon**) | **always 0 - no `std::process::exit` anywhere, even on FAIL** |
| `contract-forge` | `--dir` only (`bin/contract-forge.rs:125`); `--ops/--window/--retain` in the doc header are NOT parsed (constants `OPS=136`, `WINDOW=32` at `:38-39`) | no | real `Engine::open`, generated keys, a COPY of a real store | `RESULT: ALL PROBES PASSED` (`:971`) or `RESULT: {n} PROBE(S) FAILED` (`:973`) | 1 on failure, else 0 |
| `contract-persist` | `--dir`, `--ops` (2000), `--window` (64), `--retain` (4), `--state` (`bin/contract-persist.rs:183-188`) | YES, optional; the only bin where an empty value means SYNTHETIC targets (`:74-96`) | `{"databases":[{"id":...}]}`; generated `integrity_signing.key` 0600; re-execs itself as a second OS process with `HIVE_CONTRACT_PERSIST_PHASE=reopen` (`:547-558`) | `RESULT: ALL PROBES PASSED` (`:586`) or `RESULT: {n} PROBE(S) FAILED` (`:588`) | 1 on failure, else 0 |
| `contract-reconcile` | `--state` (`bin/contract-reconcile.rs:112`) | YES, optional, default `/tmp/vitra-real-state.json` | `Snapshot { databases, database_tombstones, database_data.write_plane }` (`:55-66`); `connection` never read | `declared={n} wired={n}`, `attested keys`, `divergences`, `contract-only`; `ALL PROBES PASSED` (`:686`) or `{n} PROBE(S) FAILED` (`:688`); **no `RESULT:` line** | 2 read/parse, 1 replay breach or probe failure, 0 pass |
| `contract-replicate` | `--state` (`bin/contract-replicate.rs:194`) | YES, optional, default `/tmp/vitra-real-state.json` | `Snapshot { databases, database_tombstones }` (`:37-43`); real `Replica::merge` / `adopt_wholesale`; `reconcile::reconcile` | `ALL PROBES PASSED` (`:725`) or `{n} PROBE(S) FAILED` (`:727`); **no `RESULT:` line** | 2 read/parse, 1 no live dbs or failures, 0 pass |
| `contract-billing` | `--dir`, `--state` (`bin/contract-billing.rs`, `fn arg`); the state path is INFORMATIONAL (byte count only), NOT a pass/fail gate | YES, optional | real `Engine::open`, real `KeyedSigner`, real `Store`; G1-G3 deserialized-contract guard, R1-R6 replay/signature/inclusion/rewrite/omission/chain-tamper probes | `RESULT PASS probes=11 ...` / `RESULT FAIL failed={n} of {n}` (space, no colon) | 0 / 1 via explicit `std::process::exit` |
| `contract-transparency` | none (`bin/contract-transparency.rs:16` documents a `--dir` that is not parsed; fixed `/private/tmp/hive-contract-transparency-<pid>`, `:64-69`) | no | real `Engine::open`, generated keys, T1-T6 inclusion/fork/availability/breach probes (`:9-14`) | `RESULT: ALL PROBES PASSED` (`:708`) or `RESULT: BREACH -- {n} guarantee(s) did not hold` (`:711`) | 0 / 1 (`:709`, `:712`) |
| `contract-double-entry` | `--dir`, `--state`, `--report` (`bin/contract-double-entry.rs`, `fn arg`) | YES, optional; **absent = S1 SKIPPED informational PASS**, present = HARD GATE over a real full `PlatformSnapshot` (unparsable/missing required keys => FAIL) | balanced double-entry journal, 17 typed invariant admit/refuse probes, reversal-only correction, 7 line/entry tamper probes, contract+journal replay, signed range commitment, node-local SOC 2 technical evidence, independent verifier, single-vs-group timing | `RESULT PASS probes=27 invariant_probes=17 replay_ok=true signed=11 proofs=8 tamper_detected=7/7 state_round_trip=true commitment_verified=true evidence_verified=true ...` (space, no colon) / `RESULT FAIL failed={n} of {n}` | 0 / 1 via explicit `std::process::exit` |

Only FIVE binaries print a `RESULT:`-prefixed line. `contract-cost`, `contract-billing` and
`contract-double-entry` print `RESULT PASS` / `RESULT FAIL` (space, no colon); unlike
`contract-cost`, the latter two exit 1 on FAIL. `contract-audit`, `contract-reconcile` and
`contract-replicate` print `ALL PROBES PASSED` / `PROBE(S) FAILED` without a `RESULT` prefix.
A monitor that greps only for `RESULT:` silently misses five of ten tools. `contract-double-entry`'s
evidence report is NODE-LOCAL technical evidence supporting an assessment, never an attestation,
never a certification and never a compliance verdict; it emits no verdict word and its
`evidence.rs` self-guard rejects any unearned `COMPLIANT` element. The documented-but-unimplemented
flags are tracked in `contract-cli-documented-flags-unimplemented`.

## 2. What `--state` actually reads

The four `--state` binaries read a real `state.json` snapshot but NOT its secrets: `RealDb` is
id/project/team/kind/status/region/created_ms only. `connection` is deliberately absent from every
`RealDb` struct (`contract-audit.rs:9`, `:31-45`; `contract-reconcile.rs:11`;
`contract-replicate.rs:11`). `contract-reconcile` additionally reads `database_tombstones` and
`database_data.write_plane`; `contract-replicate` reads tombstones. `contract-persist::targets()`
reads only `databases[].id` and falls back to synthetic `db_witness_####` names when parsing fails
(`contract-persist.rs:74-96`).

Copying a real `state.json` off a node remains an operator action: it is the canonical state file
and may carry other material. Never print it wholesale. The verification binaries only need the
fields above; a hand-built fixture with those fields is a valid input.

## 3. On-disk format

Root: `$HIVE_DATA/contract-oplog`, set in `crates/hive-cloud/src/state.rs:898`, `:902-911`.
Per-contract directory: `crates/hive-contract/src/engine.rs:71-84` (`dir_for`, sanitized single
path component). Layout (`persist.rs:19-25`):

```
contract-oplog/<contract-id>/seg-000000000000.jsonl
contract-oplog/<contract-id>/seg-000000000256.jsonl
contract-oplog/<contract-id>/cp-000000000256.json
```

`segment_name(base)` = `seg-{base:012}.jsonl` (`persist.rs:224-226`);
`checkpoint_name(end)` = `cp-{end:012}.json` (`:229-231`). Each JSONL line is a `SegmentLine`
(`{"entry": <LogEntry>, "sig": "<hex Ed25519>"}`, `persist.rs:202-207`, verified by
`verify_line` at `:421-444`). The signed preimage is `entry_sig_preimage(entry_hash, ack)`
(`persist.rs:215-221`), which folds `Ack::canonical()` in - see `docs/verifiable-database.md`
section 3. Checkpoint files are pretty-JSON `SignedCheckpoint` (`:970-973`).

Defaults: `DEFAULT_CHECKPOINT_EVERY = 256` (`persist.rs:49`), `DEFAULT_RETAIN_CHECKPOINTS = 4`
(`:54`). `DEFAULT_SHARDS` is exported from `engine.rs` and used by `Engine::open`
(`crates/hive-cloud/src/state.rs:902`).

## 4. Environment knobs

| variable | default | effect | cite |
|---|---|---|---|
| `HIVE_CONTRACT_ENTRY_SYNC` | off | `1`/`true`/`TRUE`/`yes` enables per-entry fsync; ~5.5 ms/op vs ~0.08 ms, power-loss durability, an operator decision | `state.rs:884-896`, `:335` |
| `HIVE_CONTRACT_FLUSH_SECS` | 5 (min 1) | periodic `Engine::flush()` on the platform bulkhead; bounds unflushed exposure | `crates/hive-cloud/src/main.rs:1168-1185` |
| `HIVE_CONTRACT_PERSIST_PHASE` | unset | `contract-persist` re-execs itself as the reopen phase | `bin/contract-persist.rs:44`, `:547-558` |

With per-entry fsync off, durability is still on: the segment flushes on the timer, before every
window close and before shutdown (`main.rs:1291`, `:1300-1304`). A restart rebuilds from the newest
VERIFIED checkpoint plus the tail; an unverifiable stored checkpoint or entry is refused rather
than adopted (`state.rs:328-337`). Checkpoints are signed with the node's existing
`IntegritySigner` Ed25519 keypair - the same file, resolved by the same fail-closed `open_or_create`,
so there is no second key to rotate.

## 5. Runbook

From the workspace root, with the user's PATH (Homebrew cargo has no cross std):

```bash
cargo run -p hive-contract --bin contract-transparency
cargo run -p hive-contract --bin contract-adversary
cargo run -p hive-contract --bin contract-forge
cargo run -p hive-contract --bin contract-cost
cargo run -p hive-contract --bin contract-persist
cargo run -p hive-contract --bin contract-audit -- --state /path/to/state.json
cargo run -p hive-contract --bin contract-reconcile -- --state /path/to/state.json
cargo run -p hive-contract --bin contract-replicate -- --state /path/to/state.json
```

On macOS put scratch directories under `/private/tmp`; the binaries that take `--dir` default there
or under `/tmp`. Check `df -h` before a build: the target directory is large.

Interpretation:

- `RESULT: ALL PROBES PASSED` / `ALL PROBES PASSED` / `RESULT PASS` means the probes held on the
  generated or supplied inputs. It is a live witness of the crate's mechanics, NOT a proof that
  any particular production node's log is honest.
- `contract-reconcile` prints `declared={n} wired={n}` from `DECLARED_SCOPE`
  (`crates/hive-contract/src/reconcile.rs:100-128`, `:424`). Scope `line` values are HINTS only
  (`:58-62`) and drift from the tree; `measure()` (`:178-212`) reads the source. Trust `measure`,
  not the literal line numbers.
- `contract-cost` exits 0 even on `RESULT FAIL`; read its output, not its status.
- `contract-audit` exits 3 when the snapshot has no databases, and 2 on an unreadable or
  unparsable file.

## 6. What these tools do NOT establish

- No consensus, finality or freshness. An inclusion proof says an entry WAS published, never that
  the log still ends there (`inclusion.rs:31-34`).
- Verification with no pinned key is structural only (`inclusion.rs:193-196`; `checkpoint.rs:189-191`).
- The generated keys in `contract-adversary`, `contract-forge` and `contract-transparency` are not
  the production `IntegritySigner`; they test the algorithms, not the fleet's identity.
- No bond, slashing, challenge window or punishment follows a `Dishonest` verdict (`fraud.rs:26-28`).
- The four `--state` tools read a snapshot of canonical state; they do not prove that state is
  correct, only that the contract's index and the projection can be compared.

---

See also: `docs/smart-contracts.md`, `docs/verifiable-database.md`,
`docs/contract-platform-seam.md`.
