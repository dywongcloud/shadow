// Canonical browser-artifact policy normalization + digest — the ONE JS
// implementation of the byte contract in `fluid_core::browser_policy_digest`
// (domain "hive-browser-policy-v1\0", LE fields, sorted deduped op ids with
// their ABI strings). Both runtimes import from here: the DOM-only
// BrowserFunctionRuntime (function-runtime.js, ABI strings supplied by its
// registered ops) and the worker-native WorkerFunctionRuntime
// (worker-function-runtime.js, ABI strings from HOST_OP_ABI below). Anything
// that computes a policy digest anywhere else is a drift bug: a digest that
// disagrees with the Rust side silently breaks every artifact pin.
//
// Worker-safe: no DOM, no `document`, no `location` — importable from a
// SharedWorker.

export const DIGEST_RE = /^[0-9a-f]{64}$/;

const encoder = new TextEncoder();

// Mirror of `fluid_core::browser_host_op_abi` — op id → canonical ABI string.
// The canonical policy encoding commits to the ABI TEXT, so this table must
// stay byte-identical to the Rust registry; an id missing here makes a policy
// unresolvable and therefore loudly rejected, exactly like the Rust side.
export const HOST_OP_ABI = Object.freeze(
  new Map([
    [1, "hive-browser/identity-json-v1"],
    [2, "hive-browser/utf8-array-buffer-v1"],
    [16, "hive.node-compat.fs-read/v1"],
  ]),
);

// abiFor variant backed by the platform registry (worker side). The DOM
// runtime instead resolves ABIs from its registered ops so an unregistered
// op keeps failing with ITS error ("must be registered before pin").
export function registryAbiFor(id) {
  const abi = HOST_OP_ABI.get(id);
  if (abi === undefined) {
    throw new Error(`operation ${id} is not in the platform browser host-op registry`);
  }
  return abi;
}

export function positiveInteger(value, fallback, name) {
  const out = value ?? fallback;
  if (!Number.isSafeInteger(out) || out <= 0) throw new Error(`${name} must be a positive integer`);
  return out;
}

export function sourceDigest(hash, source) {
  return hash(encoder.encode(source));
}

export function sourceDigestBytes(hash, bytes) {
  return hash(bytes);
}

function hexBytes(hex) {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

// One canonical normalization. `abiFor(id)` resolves each allowed op's ABI
// string (and throws the CALLER's preferred error for an unresolvable id —
// see the two callers above). Validation order is load-bearing for identical
// failure behaviour: mode, then ids (shape, then resolvability), then limits.
export function normalizePolicy(options, abiFor) {
  const mode = options.mode ?? "native";
  if (mode !== "native" && mode !== "quickjs") throw new Error("artifact mode must be native or quickjs");
  const ids = [...new Set(options.allowedOps ?? [])].sort((a, b) => a - b);
  const abis = new Map();
  for (const id of ids) {
    if (!Number.isSafeInteger(id) || id < 0) throw new Error("operation ids must be non-negative integers");
    abis.set(id, abiFor(id));
  }
  return Object.freeze({
    mode,
    ids: Object.freeze(ids),
    abis,
    timeoutMs: positiveInteger(options.timeoutMs, 1000, "timeoutMs"),
    memoryBytes: positiveInteger(options.memoryBytes, 32 * 1024 * 1024, "memoryBytes"),
    stackBytes: positiveInteger(options.stackBytes, 512 * 1024, "stackBytes"),
  });
}

// BLAKE3 of the canonical policy encoding — byte-for-byte
// `fluid_core::browser_policy_digest`:
//   "hive-browser-policy-v1\0" || source_digest(32 raw bytes) || u32le op_count
//   || per op (ascending id): u64le id || u8 effect(0=read) || u32le abi_len || abi
//   || u8 mode (0=native, 1=quickjs) || u64le timeout_ms || u64le memory_bytes
//   || u64le stack_bytes
export function policyDigest(hash, sourceDigestValue, policy) {
  const domain = encoder.encode("hive-browser-policy-v1\0");
  const operationBytes = policy.ids.map(id => {
    return { id, effect: 0, abi: encoder.encode(policy.abis.get(id)) };
  });
  const size = domain.length + 32 + 4
    + operationBytes.reduce((sum, item) => sum + 8 + 1 + 4 + item.abi.length, 0)
    + 1 + 8 * 3;
  const bytes = new Uint8Array(size);
  const view = new DataView(bytes.buffer);
  let offset = 0;
  bytes.set(domain, offset); offset += domain.length;
  bytes.set(hexBytes(sourceDigestValue), offset); offset += 32;
  view.setUint32(offset, operationBytes.length, true); offset += 4;
  for (const item of operationBytes) {
    view.setBigUint64(offset, BigInt(item.id), true); offset += 8;
    view.setUint8(offset, item.effect); offset += 1;
    view.setUint32(offset, item.abi.length, true); offset += 4;
    bytes.set(item.abi, offset); offset += item.abi.length;
  }
  view.setUint8(offset, policy.mode === "native" ? 0 : 1); offset += 1;
  for (const value of [policy.timeoutMs, policy.memoryBytes, policy.stackBytes]) {
    view.setBigUint64(offset, BigInt(value), true); offset += 8;
  }
  return hash(bytes);
}
