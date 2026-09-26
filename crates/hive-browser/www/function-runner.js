// One pinned artifact's node-worker host and its invocation queue
// (bn-node-worker-substrate / execution-path-swap-off-quickjs).
//
// The DOM lane's runner. It used to spawn the QuickJS function-worker bundle
// inside a sandboxed cross-site iframe; the substrate is now vendored
// node-worker, whose filesystem host must be a PAGE (navigator.serviceWorker is
// `[Exposed=Window]`, and a sandboxed iframe without `allow-same-origin` does
// not have it either), so the host is created HERE, in the page that owns the
// runtime, and the iframe is gone.
//
// WHAT REPLACES THE IFRAME, STATED HONESTLY. The guest is a dedicated
// node-worker Worker realm with the real Node module set, an EMPTY
// `process.env`, a memory filesystem holding only the artifact's own two files
// and no network transport configured. It is a separate realm from this page
// and from the trusted worker, but it is SAME-ORIGIN with the page that
// created it — the opaque-origin isolation the sandboxed frame provided is not
// something this substrate can offer. See node-worker-host.js's header for the
// full boundary, including what is still NOT mediated.

import { NodeWorkerHost } from "./node-worker-host.js";

function validOperationId(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

export class FunctionRunner {
  constructor(runtime, artifact) {
    this.runtime = runtime;
    this.artifact = artifact;
    this.pending = new Map();
    this.queue = [];
    this.nextId = 1;
    this.busy = false;
    this.closed = false;
    this.host = undefined;
  }

  async boot() {
    try {
      await this.start();
      return this;
    } catch (error) {
      this.close(error);
      throw error;
    }
  }

  async start() {
    // Boot is a real Worker fetch plus a service-worker registration: bounded
    // here because a host that never answers would otherwise hold every queued
    // invocation open forever.
    const timer = setTimeout(() => {
      this.close(new Error("function runner boot timed out"));
    }, this.runtime.bootTimeoutMs);
    try {
      const host = await NodeWorkerHost.boot({
        ...this.runtime.hostUrls,
        artifactSource: this.artifact.source,
        timeoutMs: this.artifact.timeoutMs,
        bootTimeoutMs: this.runtime.bootTimeoutMs,
        onOp: ({ op, payload }) => this.runOp(op, payload),
      });
      if (this.closed) {
        host.close();
        throw new Error("function runner closed while the node worker was booting");
      }
      this.host = host;
    } finally {
      clearTimeout(timer);
    }
  }

  invoke(request) {
    if (this.closed) return Promise.reject(new Error("function runner is closed"));
    if (this.queue.length + Number(this.busy) >= this.runtime.maxQueue) {
      return Promise.reject(new Error("function invocation queue is full"));
    }
    return new Promise((resolve, reject) => {
      this.queue.push({ request, resolve, reject });
      this.pump();
    });
  }

  pump() {
    if (this.closed || this.busy || this.queue.length === 0 || !this.host) return;
    this.busy = true;
    const item = this.queue.shift();
    const id = this.nextId++;
    // One invocation at a time and one deadline: node-worker meters neither
    // memory nor stack, so the wall clock is the only bound, and a guest that
    // outlives it is terminated by the host.
    const timer = setTimeout(() => {
      const current = this.pending.get(id);
      if (!current) return;
      this.pending.delete(id);
      current.reject(new Error("function invocation timed out"));
      this.close(new Error("function runner terminated after timeout"));
    }, this.artifact.timeoutMs);
    this.pending.set(id, { ...item, timer });
    this.host.invoke(item.request, this.artifact.timeoutMs).then(
      value => this.settle(id, true, value),
      error => this.settle(id, false, String(error?.message || error)),
    );
  }

  settle(id, ok, value) {
    const item = this.pending.get(id);
    if (!item) return;
    clearTimeout(item.timer);
    this.pending.delete(id);
    this.busy = false;
    if (ok) item.resolve(value);
    else item.reject(new Error(value));
    this.pump();
  }

  // One host operation. Dispatched HERE, never in the guest: an op the
  // artifact's `allowed_ops` does not name is refused without ever running,
  // and the handler is one this runtime registered.
  async runOp(op, payload) {
    if (!validOperationId(op)) return { ok: false, error: `operation ${op} is not a valid op id` };
    const operation = this.artifact.ops.get(op);
    if (!this.artifact.allowedOps.has(op)) return { ok: false, error: `operation ${op} is denied` };
    if (!operation) return { ok: false, error: `operation ${op} is unavailable` };
    if (!this.runtime.beginOp()) return { ok: false, error: "host operation concurrency is full" };
    try {
      const value = await operation.handler(payload, {
        digest: this.artifact.digest,
        sourceDigest: this.artifact.sourceDigest,
      });
      return { ok: true, value };
    } catch (error) {
      return { ok: false, error: String(error?.message || error) };
    } finally {
      this.runtime.endOp();
    }
  }

  close(error = new Error("function runner closed")) {
    if (this.closed) return;
    this.closed = true;
    try {
      this.host?.close();
    } catch {
      /* already gone */
    }
    this.host = undefined;
    for (const item of this.pending.values()) {
      clearTimeout(item.timer);
      item.reject(error);
    }
    for (const item of this.queue) item.reject(error);
    this.pending.clear();
    this.queue.length = 0;
    this.runtime.runnerClosed(this.artifact, this);
  }
}
