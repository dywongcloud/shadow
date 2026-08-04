import { FunctionRunner, post } from "./function-runner.js";
import {
  DIGEST_RE,
  normalizePolicy as normalizePolicyShape,
  policyDigest,
  positiveInteger,
  sourceDigest,
} from "./artifact-policy.js";

const encoder = new TextEncoder();

function site(hostname) {
  if (hostname === "localhost" || /^\d+(?:\.\d+){3}$/.test(hostname)) return hostname;
  return hostname.split(".").slice(-2).join(".");
}

function abortError(signal) {
  return signal.reason instanceof Error ? signal.reason : new Error("operation aborted");
}

function abortable(promise, signal) {
  if (signal.aborted) return Promise.reject(abortError(signal));
  return new Promise((resolve, reject) => {
    const onAbort = () => reject(abortError(signal));
    signal.addEventListener("abort", onAbort, { once: true });
    Promise.resolve(promise).then(
      value => {
        signal.removeEventListener("abort", onAbort);
        resolve(value);
      },
      error => {
        signal.removeEventListener("abort", onAbort);
        reject(error);
      },
    );
  });
}

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
    this.development = options.development === true;
    const configuredFrame = options.frameUrl;
    if (!this.development && !configuredFrame) throw new Error("production function runtime requires frameUrl");
    const frameUrl = new URL(configuredFrame || "./function-frame.html", location.href);
    if (!this.development) {
      if (frameUrl.protocol !== "https:") throw new Error("production function frame must use https");
      if (frameUrl.origin === location.origin || site(frameUrl.hostname) === site(location.hostname)) {
        throw new Error("production function frame must use a separate site");
      }
    }
    this.frameUrl = frameUrl.href;
    this.workerUrl = new URL(options.workerUrl || "./pkg/function-worker.js", location.href).href;
    this.bootTimeoutMs = positiveInteger(options.bootTimeoutMs, 10000, "bootTimeoutMs");
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
    this.opCompletions = [];
    this.opFlushScheduled = false;
    this.activeOps = 0;
    this.closed = false;
    this.handleInvoke = this.handleInvoke.bind(this);
  }

  loadWorkerSource(signal) {
    if (this.closed) return Promise.reject(new Error("function runtime is closed"));
    if (!this.workerSource) {
      const controller = new AbortController();
      this.workerSourceController = controller;
      const timer = setTimeout(() => {
        controller.abort(new Error("function worker fetch timed out"));
      }, this.bootTimeoutMs);
      let source;
      source = fetch(this.workerUrl, { signal: controller.signal })
        .then(async response => {
          if (!response.ok) throw new Error(`function worker fetch failed: ${response.status}`);
          return response.text();
        })
        .catch(error => {
          if (controller.signal.aborted) throw abortError(controller.signal);
          throw error;
        })
        .finally(() => {
          clearTimeout(timer);
          if (this.workerSourceController === controller) this.workerSourceController = undefined;
        });
      source.catch(() => {
        if (this.workerSource === source) this.workerSource = undefined;
      });
      this.workerSource = source;
    }
    return abortable(this.workerSource, signal);
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
      timeoutMs: policy.timeoutMs,
      allowedOps: new Set(policy.ids),
      ops: policy.operations,
      runner: undefined,
      ready: undefined,
      workerConfig: {
        source,
        mode: policy.mode,
        limits: {
          memoryBytes: policy.memoryBytes,
          stackBytes: policy.stackBytes,
        },
      },
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

  scheduleOpFlush() {
    if (this.opFlushScheduled) return;
    this.opFlushScheduled = true;
    queueMicrotask(() => {
      this.opFlushScheduled = false;
      const byRunner = new Map();
      for (const item of this.opCompletions.splice(0)) {
        if (!item.runner.closed) {
          const batch = byRunner.get(item.runner) || [];
          batch.push(item.result);
          byRunner.set(item.runner, batch);
        }
      }
      for (const [runner, items] of byRunner) post(runner.port, { kind: "opBatch", items });
    });
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    this.workerSourceController?.abort(new Error("function runtime closed"));
    for (const artifact of this.artifacts.values()) artifact.runner?.close();
    this.artifacts.clear();
  }
}
