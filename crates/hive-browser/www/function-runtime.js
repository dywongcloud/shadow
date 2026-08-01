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
    this.workerSource = fetch(this.workerUrl).then(async response => {
      if (!response.ok) throw new Error(`function worker fetch failed: ${response.status}`);
      return response.text();
    });
    this.bootTimeoutMs = options.bootTimeoutMs || 10000;
    this.maxQueue = options.maxQueue || 32;
    if (typeof options.blake3 !== "function") throw new Error("function runtime requires a BLAKE3 implementation");
    this.blake3 = options.blake3;
    this.ops = new Map(Object.entries(options.ops || {}).map(([id, fn]) => [Number(id), fn]));
    this.artifacts = new Map();
    this.opCompletions = [];
    this.opFlushScheduled = false;
    this.closed = false;
    this.handleInvoke = this.handleInvoke.bind(this);
  }

  async pin(digest, source, options = {}) {
    if (this.closed) throw new Error("function runtime is closed");
    if (!DIGEST.test(digest)) throw new Error("artifact digest must be 64 lowercase hexadecimal characters");
    if (typeof source !== "string") throw new Error("artifact source must be a string");
    if (sourceDigest(this.blake3, source) !== digest) throw new Error("artifact source does not match BLAKE3 digest");
    const mode = options.mode || "native";
    if (mode !== "native" && mode !== "quickjs") throw new Error("artifact mode must be native or quickjs");
    const allowedOps = new Set(options.allowedOps || []);
    for (const op of allowedOps) {
      if (!Number.isSafeInteger(op) || op < 0) throw new Error("operation ids must be non-negative integers");
    }
    const artifact = {
      digest,
      timeoutMs: options.timeoutMs || 1000,
      allowedOps,
      runner: undefined,
      ready: undefined,
      workerConfig: {
        source,
        mode,
        limits: {
          memoryBytes: options.memoryBytes || 32 * 1024 * 1024,
          stackBytes: options.stackBytes || 512 * 1024,
        },
      },
    };
    this.artifacts.get(digest)?.runner?.close(new Error("artifact replaced"));
    this.artifacts.set(digest, artifact);
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

  setOp(id, handler) {
    if (!Number.isSafeInteger(id) || id < 0) throw new Error("operation id must be a non-negative integer");
    if (typeof handler !== "function") throw new Error("operation handler must be a function");
    this.ops.set(id, handler);
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
    for (const artifact of this.artifacts.values()) artifact.runner?.close();
    this.artifacts.clear();
  }
}
