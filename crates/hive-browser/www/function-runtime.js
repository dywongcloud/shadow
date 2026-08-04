import { FunctionRunner, post } from "./function-runner.js";

const DIGEST = /^[0-9a-f]{64}$/;
const encoder = new TextEncoder();

function site(hostname) {
  if (hostname === "localhost" || /^\d+(?:\.\d+){3}$/.test(hostname)) return hostname;
  return hostname.split(".").slice(-2).join(".");
}

function sourceDigest(hash, source) {
  return hash(encoder.encode(source));
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

function positiveInteger(value, fallback, name) {
  const out = value ?? fallback;
  if (!Number.isSafeInteger(out) || out <= 0) throw new Error(`${name} must be a positive integer`);
  return out;
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

function hexBytes(hex) {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

function normalizePolicy(options, operations) {
  const mode = options.mode ?? "native";
  if (mode !== "native" && mode !== "quickjs") throw new Error("artifact mode must be native or quickjs");
  const ids = [...new Set(options.allowedOps ?? [])].sort((a, b) => a - b);
  const snapshot = new Map();
  for (const id of ids) {
    if (!Number.isSafeInteger(id) || id < 0) throw new Error("operation ids must be non-negative integers");
    const operation = operations.get(id);
    if (!operation) throw new Error(`operation ${id} must be registered before pin`);
    snapshot.set(id, operation);
  }
  return Object.freeze({
    mode,
    ids: Object.freeze(ids),
    operations: snapshot,
    timeoutMs: positiveInteger(options.timeoutMs, 1000, "timeoutMs"),
    memoryBytes: positiveInteger(options.memoryBytes, 32 * 1024 * 1024, "memoryBytes"),
    stackBytes: positiveInteger(options.stackBytes, 512 * 1024, "stackBytes"),
  });
}

function policyDigest(hash, sourceDigestValue, policy) {
  const domain = encoder.encode("hive-browser-policy-v1\0");
  const operationBytes = policy.ids.map(id => {
    const operation = policy.operations.get(id);
    return { id, effect: 0, abi: encoder.encode(operation.abi) };
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
    if (!DIGEST.test(sourceDigestValue)) throw new Error("artifact digest must be 64 lowercase hexadecimal characters");
    if (typeof source !== "string") throw new Error("artifact source must be a string");
    if (sourceDigest(this.blake3, source) !== sourceDigestValue) throw new Error("artifact source does not match BLAKE3 digest");
    const policy = normalizePolicy(options, this.ops);
    const digest = policyDigest(this.blake3, sourceDigestValue, policy);
    if (!DIGEST.test(digest)) throw new Error("BLAKE3 policy digest must be 64 lowercase hexadecimal characters");
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
