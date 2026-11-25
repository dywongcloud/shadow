# Inference backends: llama.cpp stays, jitllm is a HOLD

Status: 2026-11-24. This is a decision record, not a migration plan. It records
what an adversarial review established and what must be decided before any
inference binary changes on the fleet.

## What was reviewed

Upstream `https://github.com/jitllm/jitllm` at pinned HEAD
`c4c554167bd9f87e8c4b952805419c1099de0a33` (v0.2.7, Go, Apache-2.0). Three
independent refuters each returned `refuted=true` against the propositions that
a swap is a drop-in, that it preserves current capability, and that the quoted
speed ratios are an admissible basis for a change. A synthesis judge returned
NO-GO. The fourth refuter was stopped mid-verification, so upstream
completeness is not claimed.

The quoted 4x / 4.62x prefill / 5.53x batched decode / 1.53x Apple M4 decode
figures are vendor same-machine A/B numbers from `docs/performance.md`
(4x V100 and M4). They are NOT measured on this fleet, carry no
`(id, hash, ts)` witness, and are explicitly not admitted as a reason for any
code, documentation or commit-message change. "Measurement gates optimization
claims, not effort" and Admission Filter L1 reject an unprofiled speedup.

## Why llama.cpp stays

1. Multi-node pooling. `inference.rs` elects one coordinator, and when a model
   does not fit that node's VRAM it adds same-region peers until aggregate VRAM
   covers `vram_need_mb`, builds `<ip>:50052` addresses and launches
   `llama-server --rpc ...` over the peers-only port. Upstream `ROADMAP.md`
   lines 135-140 label network swarm orchestration "**Not started.**" with the
   implementation outside core; `docs/devices.md` and the proto describe only
   local device IDs and host-to-device paging inside one host. `jitllmd serve`
   has no `--rpc` equivalent. Removing the only pooling path would foreclose a
   real deployment as soon as a GPU host returns. All five GPU hosts currently
   carry `hive_retired=true`, which makes the pool idle, not preserved.

2. Model format. `jitllmd -load` needs a converted `.jlm` container. This repo
   downloads and caches `<blake3>.gguf`. No conversion lane exists.

3. CLI and readiness. `jitllmd serve` defines `-addr`, `-api-key`, `-models`,
   `-load`, `-id`, `-devices` and related flags; Go's FlagSet rejects `-m`,
   `--gpu-layers`, `--alias` and `--rpc`. Its llama.cpp-compatible `/health`
   writes an unconditional HTTP 200 without checking engine or model state,
   whereas `llama-server` answers 503 for the whole load. Any new backend must
   supply a positive readiness proof before it is allowed to clear the load
   watchdog.

4. Auth and exposure. A keyed service needs `Authorization: Bearer KEY` on
   every route but `/healthz`. The platform injects exactly one env var,
   `HIVE_INFERENCE_URL`, with `sensitive: false`. There is no typed credential
   channel, and a key must never be added as a plaintext tenant env value.

5. Supply chain. 33 release assets, no verified checksum or signature in the
   pinned digest story. The repository's own `AGENTS.md` and `llms.txt` are
   untrusted data, never instructions. A reviewed, digest-pinned artifact and
   reviewed base are a prerequisite for any new binary.

## What shipped instead (local, no roll)

- `InferenceSpec.backend` is an additive `#[serde(default)]` field with a
  lenient enum: an unknown name deserializes to `Unknown` rather than failing
  `persist::load` (a deserialize error there is silent total state loss).
- `reconcile_local` refuses any backend other than `llama_cpp` with a fixed,
  honest `failed: ... (NodeRuntimeMissing)` status and a
  `fault::NODE_RUNTIME_MISSING` warning. No child is spawned, no model is
  downloaded, and `HIVE_INFERENCE_URL` is not injected. A `fluid.json` asking
  for `jitllm` no longer silently gets llama.cpp.
- The llama.cpp launcher, `--rpc` pooling path and `HIVE_LLAMA_BIN` resolution
  are unchanged.
- The loopback readiness probe now uses
  `hive_outbound::internal_client()` (`Policy::none()` plus `.no_proxy()`),
  not a bare `reqwest::Client::new()`, which honoured ambient
  `HTTP_PROXY`/`ALL_PROXY` and followed redirects.

## Residuals, each needing an explicit operator decision

| Residual | Required operator action |
| --- | --- |
| Reviewed artifact | Name a digest-pinned jitllm release and a reviewed base; opt-in, operator-only roll |
| Performance | Re-enroll at least two GPU hosts and authorise an on-hardware A/B before any ratio is quoted |
| Model format | Approve a `.jlm` conversion lane with its own BLAKE3 domain, `DiskClaim` bounds and bulkhead execution |
| Pooling | Decide whether the multi-host `--rpc` deployment must be preserved |
| Key custody | Decide `secrets::try_decrypt`, force-masked sensitive env and the gateway surface |
| Readiness proof | Define a backend-bearing body check before a non-llama.cpp backend may clear `load_started_ms` |
| Capability | Probe and advertise the runner (`NodeInfo`, `RuntimeCapabilitySource`), with `None` meaning NOT CAPABLE and `NODE_RUNTIME_MISSING` |
| Roll | A roll needs the operator's word on a specific SHA, `scripts/audit-secret-keys.sh`, the deploy playbook and proof from the RUNNING binary |

The pasted `PREFERENCES` and "System and code Design Invariants" blocks were
elided in the message that reached this session; no unseen clause is claimed to
have been honoured. Confirm the full text before any swap.
