import { FunctionRunner } from "./function-runner.js";
import {
  DIGEST_RE,
  normalizePolicy as normalizePolicyShape,
  policyDigest,
  positiveInteger,
  sourceDigest,
} from "./artifact-policy.js";
import { assetUrls } from "./node-worker-host.js";

const encoder = new TextEncoder();

function readOperation(value) {
  const handler = typeof value === "function" ? value : value?.handler;
  const effect = typeof value === "function" ? "read" : value?.effect ?? "read";
  const abi = typeof value === "function" ? undefined : value?.abi;
  if (typeof handler !== "function") throw new Error("operation handler must be a function");
  if (effect !== "read") throw new Error("write operations require an atomic commit-fence contract");
  if (typeof abi !== "string" || !abi || encoder.encode(abi).length > 256) {
    throw new Error("operation abi must be a non-empty UTF-8 string of at most 256 bytes");
  }
  return Object.freeze({ handler, effect, abi });
}

// Policy normalization delegates the canonical shape (mode/ids/limits and the
// ABI lookup order) to artifact-policy.js; the runtime-specific part is that
// an op's ABI comes from the ops REGISTERED here, so an unregistered op keeps
// failing with this runtime's own error. `abis` is carried through for the
// shared policyDigest; `operations` for the runner's op dispatch.
function normalizePolicy(options, operations) {
  const policy = normalizePolicyShape(options, id => {
    const operation = operations.get(id);
    if (!operation) throw new Error(`operation ${id} must be registered before pin`);
    return operation.abi;
  });
  const snapshot = new Map();
  for (const id of policy.ids) snapshot.set(id, operations.get(id));
  return Object.freeze({
    mode: policy.mode,
    ids: policy.ids,
    abis: policy.abis,
    operations: snapshot,
    timeoutMs: policy.timeoutMs,
    memoryBytes: policy.memoryBytes,
    stackBytes: policy.stackBytes,
  });
}

export class BrowserFunctionRuntime {
  constructor(options = {}) {
    // The substrate is hosted in THIS page (node-worker's filesystem host needs
    // `navigator.serviceWorker`), so there is no sandboxed frame to configure
    // any more — see function-runner.js for what replaced it and what that
    // costs. `substrateBase` overrides where the vendored build was published.
    this.hostUrls = assetUrls(options.substrateBase || "./node-worker/");
    this.bootTimeoutMs = positiveInteger(options.bootTimeoutMs, 20000, "bootTimeoutMs");
    this.maxQueue = positiveInteger(options.maxQueue, 32, "maxQueue");
    this.maxActiveOps = positiveInteger(options.maxActiveOps, 32, "maxActiveOps");
    if (typeof options.blake3 !== "function") throw new Error("function runtime requires a BLAKE3 implementation");
    this.blake3 = options.blake3;
    this.ops = new Map();
    for (const [rawId, value] of Object.entries(options.ops || {})) {
      const id = Number(rawId);
      if (!Number.isSafeInteger(id) || id < 0) throw new Error("operation id must be a non-negative integer");
      this.ops.set(id, readOperation(value));
    }
    this.artifacts = new Map();
    this.activeOps = 0;
    this.closed = false;
    this.handleInvoke = this.handleInvoke.bind(this);
  }

  beginOp() {
    if (this.activeOps >= this.maxActiveOps) return false;
    this.activeOps += 1;
    return true;
  }

  endOp() {
    this.activeOps -= 1;
  }

  async pin(sourceDigestValue, source, options = {}) {
    if (this.closed) throw new Error("function runtime is closed");
    if (!DIGEST_RE.test(sourceDigestValue)) throw new Error("artifact digest must be 64 lowercase hexadecimal characters");
    if (typeof source !== "string") throw new Error("artifact source must be a string");
    if (sourceDigest(this.blake3, source) !== sourceDigestValue) throw new Error("artifact source does not match BLAKE3 digest");
    const policy = normalizePolicy(options, this.ops);
    const digest = policyDigest(this.blake3, sourceDigestValue, policy);
    if (!DIGEST_RE.test(digest)) throw new Error("BLAKE3 policy digest must be 64 lowercase hexadecimal characters");
    const artifact = {
      digest,
      sourceDigest: sourceDigestValue,
      // The verified source, handed to the substrate EXACTLY as pinned: the
      // host wraps `module.exports = (…)` around it at boot
      // (node-worker-host.js). Nothing is substituted into it, and nothing
      // needs to be — node-worker IS Node, so there is no Node API shim to
      // prepend any more.
      source,
      mode: policy.mode,
      timeoutMs: policy.timeoutMs,
      allowedOps: new Set(policy.ids),
      ops: policy.operations,
      runner: undefined,
      ready: undefined,
    };
    this.artifacts.get(digest)?.runner?.close(new Error("artifact replaced"));
    this.artifacts.set(digest, artifact);
    return digest;
  }

  unpin(digest) {
    const artifact = this.artifacts.get(digest);
    if (!artifact) return false;
    this.artifacts.delete(digest);
    artifact.runner?.close(new Error("artifact unpinned"));
    return true;
  }

  async handleInvoke(digest, request) {
    if (this.closed) throw new Error("function runtime is closed");
    const artifact = this.artifacts.get(digest);
    if (!artifact) throw new Error("artifact is not pinned locally");
    if (!artifact.runner) {
      artifact.runner = new FunctionRunner(this, artifact);
      artifact.ready = artifact.runner.boot();
    }
    const runner = await artifact.ready;
    if (this.artifacts.get(digest) !== artifact) {
      runner.close(new Error("artifact changed during runner boot"));
      throw new Error("artifact is no longer pinned");
    }
    return runner.invoke(request);
  }

  attach(node) {
    node.setInvokeHandler(this.handleInvoke);
  }

  setOp(id, handler, options = {}) {
    if (!Number.isSafeInteger(id) || id < 0) throw new Error("operation id must be a non-negative integer");
    this.ops.set(id, readOperation({ handler, effect: options.effect, abi: options.abi }));
  }

  removeOp(id) {
    return this.ops.delete(id);
  }

  runnerClosed(artifact, runner) {
    if (artifact.runner === runner) {
      artifact.runner = undefined;
      artifact.ready = undefined;
    }
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    for (const artifact of this.artifacts.values()) artifact.runner?.close();
    this.artifacts.clear();
  }
}
