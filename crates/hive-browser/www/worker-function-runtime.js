// Worker-native bounded QuickJS function runtime (browser-worker-quickjs-runtime).
// The DOM-only BrowserFunctionRuntime (function-runtime.js/function-runner.js)
// needs `document`, an iframe and page message events, so it cannot run inside
// the "Run a node" SharedWorker. This runtime is the worker-context lane:
//
//   * QUICKJS MODE ONLY. Native-mode artifacts evaluate tenant JS directly in
//     the host context — inside the trusted worker that owns the iroh endpoint,
//     the seed and the platform session that would be a privilege escape, so
//     pin() rejects them. QuickJS executes IN THIS THREAD (quickjs-emscripten
//     RELEASE_SYNC has no yield), bounded by the engine interrupt handler —
//     the only substrate with hard memory/stack/CPU limits (see
//     docs/browser-node-proposal.md §2.3).
//   * The shipped pkg/function-worker.js bundle (quickjs-emscripten + the
//     src-js runner protocol, embedded wasm) is evaluated with a shimmed
//     `self`, giving each artifact an isolated in-thread "virtual worker" that
//     speaks the EXACT same boot/invoke/opBatch/close protocol as the real
//     dedicated Worker the iframe spawns — one tested code path, no rebundled
//     variant to drift. The bundle's only global-scope dependency is `self.*`
//     (verified against the built bundle: self.location/onmessage/postMessage/
//     close; emscripten's window/document references are feature-tests that are
//     false in any worker scope).
//   * NODE API (browser-node-api-runtime). The QuickJS guest's global object is
//     bare ECMAScript — measured: no console, no TextEncoder/TextDecoder, no
//     URL, no timers, no atob/btoa, no crypto, no fetch. node-runtime.js is
//     evaluated in the guest immediately before the artifact function and
//     supplies `require`/`process`/`Buffer`/`console`/timers/`URL`/
//     `TextEncoder` plus the Node builtin module set, so a handler written for
//     Node (including an Express app or a `(req, res)` listener) runs as
//     written. It is wrapped AROUND the verified source, never merged into it:
//     pin()'s size/BLAKE3/policy-digest verification below is unchanged and
//     still runs against the exact bytes the server described.
//   * Execution budgets are capped by maxExecBlockMs BELOW the policy ceiling
//     when necessary: one synchronous guest segment blocks this worker's whole
//     event loop, including the iroh endpoint driving the relay connection, so
//     the budget handed to QuickJS is min(policy.timeoutMs, maxExecBlockMs).
//
// Queue/cap model: one bounded FIFO per artifact (single active invocation —
// the underlying runner rejects concurrent invokes), a global active cap
// (invocations started, incl. suspended-on-op) and a global queued cap.

import {
  DIGEST_RE,
  normalizePolicy,
  policyDigest,
  positiveInteger,
  registryAbiFor,
  sourceDigestBytes,
} from "./artifact-policy.js";
import { wrapArtifactSource } from "./node-runtime.js";

const QUICKJS_INTERRUPT_GRACE_MS = 50;
const ARTIFACT_SOURCE_MAX_BYTES = 512 * 1024; // build contract caps entries at 256 KiB + fixed envelope

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

function validOperationId(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

// Evaluate the function-worker bundle with `self` bound to a shim and return
// the host end of its message protocol. Worker→host messages are delivered
// synchronously (the bundle posts op/result messages from inside a
// synchronous guest segment; the host handlers only enqueue/async-dispatch,
// never reenter the guest). Host→worker `invoke` runs the guest's first
// segment SYNCHRONOUSLY inside postMessage — that is the intended in-thread,
// interrupt-bounded execution.
function createInlineWorker(bundleSource, sourceUrl, onFatal) {
  let hostMessage = null;
  let terminated = false;
  const fakeSelf = {
    location: { href: sourceUrl },
    postMessage(message) {
      if (terminated || hostMessage === null) return;
      hostMessage({ data: message });
    },
    close() {
      terminated = true;
    },
  };
  const evaluate = new Function("self", `${bundleSource}\n//# sourceURL=${sourceUrl}#inline`);
  evaluate(fakeSelf);
  if (typeof fakeSelf.onmessage !== "function") {
    throw new Error("function worker bundle did not install an onmessage handler");
  }
  return {
    set onmessage(handler) {
      hostMessage = handler;
    },
    postMessage(message) {
      if (terminated) return;
      try {
        const delivered = fakeSelf.onmessage({ data: message });
        // The bundle catches its own errors and reports them as `fatal`
        // messages; a rejection here means that error path itself failed.
        Promise.resolve(delivered).catch(error => onFatal(error));
      } catch (error) {
        onFatal(error);
      }
    },
    terminate() {
      terminated = true;
    },
  };
}

class InlineFunctionRunner {
  constructor(runtime, artifact) {
    this.runtime = runtime;
    this.artifact = artifact;
    this.pending = new Map();
    this.queue = artifact.queue;
    this.busy = false;
    this.closed = false;
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
    const controller = new AbortController();
    this.bootController = controller;
    const timer = setTimeout(() => {
      controller.abort(new Error("function runner boot timed out"));
    }, this.runtime.bootTimeoutMs);
    try {
      const workerSource = await this.runtime.loadWorkerSource(controller.signal);
      if (controller.signal.aborted) throw abortError(controller.signal);
      this.port = createInlineWorker(workerSource, this.runtime.workerUrl, error => {
        this.close(new Error(`function worker failed: ${error?.message || error}`));
      });
      this.port.onmessage = ({ data }) => this.onMessage(data);
      const booted = abortable(new Promise(resolve => {
        this.bootResolve = resolve;
      }), controller.signal);
      this.port.postMessage({
        kind: "boot",
        // The Node API surface (browser-node-api-runtime) is installed AROUND
        // the verified artifact, never inside it: `wrapArtifactSource` returns
        // an expression that installs `require`/`process`/`Buffer`/`console`/
        // timers/`URL`/`TextEncoder` in the guest and then evaluates to the
        // artifact function unchanged. The artifact BYTES are untouched — they
        // were size-checked, BLAKE3-matched and policy-digest-verified by
        // pin() before this line, and none of that is recomputed or relaxed
        // here. The runtime is substrate, exactly like `ops`: it grants no
        // capability the artifact's `allowed_ops` did not already grant.
        source: wrapArtifactSource(this.artifact.source),
        mode: "quickjs",
        limits: {
          memoryBytes: this.artifact.memoryBytes,
          stackBytes: this.artifact.stackBytes,
        },
      });
      await booted;
    } finally {
      clearTimeout(timer);
      this.bootResolve = undefined;
      if (this.bootController === controller) this.bootController = undefined;
    }
  }

  pump() {
    if (this.closed || this.busy || this.queue.length === 0) return;
    if (!this.runtime.beginInvoke()) return; // global active cap — stay queued
    this.busy = true;
    const item = this.queue.shift();
    this.runtime.globalQueued -= 1;
    const id = this.runtime.nextId++;
    const controller = new AbortController();
    // The budget actually handed to QuickJS: the policy timeout clamped to the
    // relay-liveness ceiling. The guest's interrupt handler fires at this
    // deadline; the host timer below is the backstop for an overrun (a guest
    // stuck in an uninterruptible native op can only be abandoned, and the
    // timer itself can only fire after the thread unblocks — the overshoot is
    // what the witness profiles).
    const deadlineMs = Math.min(this.artifact.timeoutMs, this.runtime.maxExecBlockMs);
    const timer = setTimeout(() => {
      const current = this.pending.get(id);
      if (!current) return;
      const error = new Error("function invocation timed out");
      current.controller.abort(error);
      current.reject(error);
      this.pending.delete(id);
      this.close(new Error("function runner terminated after timeout"));
    }, deadlineMs + QUICKJS_INTERRUPT_GRACE_MS);
    this.pending.set(id, { ...item, timer, controller, calls: new Set() });
    this.port.postMessage({ kind: "invoke", id, request: item.request, deadlineMs });
  }

  onMessage(message) {
    if (this.closed) return;
    if (message.kind === "ready") {
      this.bootResolve?.();
    } else if (message.kind === "fatal") {
      this.close(new Error(`function worker failed: ${message.error}`));
    } else if (message.kind === "result") {
      const item = this.pending.get(message.id);
      if (!item) return;
      clearTimeout(item.timer);
      item.controller.abort(new Error("function invocation completed"));
      this.pending.delete(message.id);
      this.runtime.endInvoke(this); // clears busy + de-counts the active slot
      if (message.ok) item.resolve(message.value);
      else item.reject(new Error(message.error));
      this.pump();
    } else if (message.kind === "op") {
      void this.runOp(message).catch(error => this.close(error));
    }
  }

  async runOp(message) {
    if (!validOperationId(message.id) || !validOperationId(message.call) || !validOperationId(message.op)) {
      this.close(new Error("function worker sent a malformed operation message"));
      return;
    }
    const item = this.pending.get(message.id);
    if (!item || item.calls.has(message.call)) {
      this.close(new Error("function worker sent a stale or duplicate operation message"));
      return;
    }
    item.calls.add(message.call);
    const operation = this.runtime.opHandlers.get(message.op);
    const allowed = this.artifact.allowedOps.has(message.op);
    let result;
    if (!allowed) result = { call: message.call, ok: false, error: `operation ${message.op} is denied` };
    else if (!operation) result = { call: message.call, ok: false, error: `operation ${message.op} is unavailable` };
    else if (!this.runtime.beginOp()) result = { call: message.call, ok: false, error: "host operation concurrency is full" };
    else {
      try {
        result = {
          call: message.call,
          ok: true,
          value: await operation(message.payload, {
            digest: this.artifact.digest,
            sourceDigest: this.artifact.sourceDigest,
            invocationId: message.id,
            callId: message.call,
            signal: item.controller.signal,
          }),
        };
      } catch (error) {
        result = { call: message.call, ok: false, error: String(error?.message || error) };
      } finally {
        this.runtime.endOp();
      }
    }
    if (this.closed || this.pending.get(message.id) !== item || item.controller.signal.aborted) return;
    this.runtime.opCompletions.push({ runner: this, result });
    this.runtime.scheduleOpFlush();
  }

  close(error = new Error("function runner closed")) {
    if (this.closed) return;
    this.closed = true;
    this.runtime.endInvoke(this);
    this.bootController?.abort(error);
    if (this.port) {
      try {
        this.port.postMessage({ kind: "close" });
      } catch {
        /* already gone */
      }
      this.port.terminate();
    }
    for (const item of this.pending.values()) {
      clearTimeout(item.timer);
      item.controller.abort(error);
      item.reject(error);
    }
    let dropped = 0;
    for (const item of this.queue) {
      dropped += 1;
      item.reject(error);
    }
    this.runtime.globalQueued -= dropped;
    this.pending.clear();
    this.queue.length = 0;
    this.runtime.runnerClosed(this.artifact, this);
  }
}

export class WorkerFunctionRuntime {
  constructor(options = {}) {
    if (typeof options.blake3 !== "function") throw new Error("worker function runtime requires a BLAKE3 implementation");
    this.blake3 = options.blake3;
    this.workerUrl = new URL(options.workerUrl || "./pkg/function-worker.js", import.meta.url).href;
    this.bootTimeoutMs = positiveInteger(options.bootTimeoutMs, 10000, "bootTimeoutMs");
    this.maxQueuePerArtifact = positiveInteger(options.maxQueuePerArtifact, 32, "maxQueuePerArtifact");
    this.maxQueuedGlobal = positiveInteger(options.maxQueuedGlobal, 64, "maxQueuedGlobal");
    this.maxActiveGlobal = positiveInteger(options.maxActiveGlobal, 4, "maxActiveGlobal");
    this.maxActiveOps = positiveInteger(options.maxActiveOps, 32, "maxActiveOps");
    // Relay-liveness ceiling for ONE synchronous guest segment. Defaults to
    // the platform's own default policy timeout; sized by the live interrupt
    // profile (see the row's witness: relay echo survives a 1s block with
    // margin, and the iroh event loop resumes within ms of the interrupt).
    this.maxExecBlockMs = positiveInteger(options.maxExecBlockMs, 1000, "maxExecBlockMs");
    this.opHandlers = new Map();
    for (const [rawId, handler] of Object.entries(options.ops || {})) this.setOp(Number(rawId), handler);
    this.artifacts = new Map();
    this.opCompletions = [];
    this.opFlushScheduled = false;
    this.activeOps = 0;
    this.globalActive = 0;
    this.globalQueued = 0;
    this.nextId = 1;
    this.servedTotal = 0;
    this.closed = false;
  }

  // Only EXPLICIT host ops from the platform registry can ever be dispatched;
  // per artifact the admission policy's allowed_ops narrows that further.
  setOp(id, handler) {
    if (!Number.isSafeInteger(id) || id < 0) throw new Error("operation id must be a non-negative integer");
    if (typeof handler !== "function") throw new Error("operation handler must be a function");
    registryAbiFor(id); // unknown platform op — refuse to register at all
    this.opHandlers.set(id, handler);
  }

  loadWorkerSource(signal) {
    if (this.closed) return Promise.reject(new Error("worker function runtime is closed"));
    if (!this.workerSource) {
      const controller = new AbortController();
      this.workerSourceController = controller;
      const timer = setTimeout(() => {
        controller.abort(new Error("function worker fetch timed out"));
      }, this.bootTimeoutMs);
      const source = fetch(this.workerUrl, { signal: controller.signal })
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

  has(digest) {
    return this.artifacts.has(digest);
  }

  descriptor(digest) {
    const artifact = this.artifacts.get(digest);
    if (!artifact) return undefined;
    return {
      digest: artifact.digest,
      sourceDigest: artifact.sourceDigest,
      sourceBytes: artifact.sourceBytes,
      timeoutMs: artifact.timeoutMs,
      memoryBytes: artifact.memoryBytes,
      stackBytes: artifact.stackBytes,
      allowedOps: [...artifact.allowedOps],
      queued: artifact.queue.length,
      busy: artifact.runner?.busy === true,
      served: artifact.served,
    };
  }

  // Pin ONLY server-described, locally re-verified source. `descriptor` is the
  // admission capability block; `sourceBytes` the fetched (or cache-read)
  // artifact body. Verification chain, all local, all before anything runs:
  //   1. shape: digests are 64-hex, limits are positive, ops resolve in the
  //      platform registry, mode is quickjs (native would run tenant JS
  //      unsandboxed in this trusted worker — rejected, never executed);
  //   2. byte length matches descriptor.source_bytes;
  //   3. BLAKE3(bytes) matches descriptor.source_digest;
  //   4. the canonical policy digest recomputed from (source_digest, mode,
  //      limits, allowed_ops) matches descriptor.policy_digest — the
  //      byte-identical encoding the Rust build contract used, so a stale or
  //      mismatched descriptor is detected exactly.
  pin(descriptor, sourceBytes) {
    if (this.closed) throw new Error("worker function runtime is closed");
    if (!descriptor || typeof descriptor !== "object") throw new Error("artifact descriptor is required");
    const { policyDigest: policyDigestValue, sourceDigest: sourceDigestValue } = descriptor;
    if (!DIGEST_RE.test(policyDigestValue || "")) throw new Error("policy digest must be 64 lowercase hexadecimal characters");
    if (!DIGEST_RE.test(sourceDigestValue || "")) throw new Error("source digest must be 64 lowercase hexadecimal characters");
    if (descriptor.mode !== "quickjs") {
      throw new Error(`artifact mode ${JSON.stringify(descriptor.mode)} is unsupported — only quickjs artifacts run in the worker runtime`);
    }
    const policy = normalizePolicy({
      mode: descriptor.mode,
      timeoutMs: descriptor.timeoutMs,
      memoryBytes: descriptor.memoryBytes,
      stackBytes: descriptor.stackBytes,
      allowedOps: descriptor.allowedOps,
    }, registryAbiFor);
    const computed = policyDigest(this.blake3, sourceDigestValue, policy);
    if (computed !== policyDigestValue) {
      throw new Error("descriptor does not match the canonical policy digest — stale or mismatched capability");
    }
    if (!(sourceBytes instanceof Uint8Array)) throw new Error("artifact source bytes must be a Uint8Array");
    if (sourceBytes.length > ARTIFACT_SOURCE_MAX_BYTES) {
      throw new Error(`artifact source is ${sourceBytes.length} bytes, over the ${ARTIFACT_SOURCE_MAX_BYTES}-byte bound`);
    }
    if (sourceBytes.length !== descriptor.sourceBytes) {
      throw new Error(`artifact source is ${sourceBytes.length} bytes, descriptor declares ${descriptor.sourceBytes}`);
    }
    if (sourceDigestBytes(this.blake3, sourceBytes) !== sourceDigestValue) {
      throw new Error("artifact bytes do not match the descriptor's BLAKE3 source digest");
    }
    const source = new TextDecoder().decode(sourceBytes);
    const existing = this.artifacts.get(policyDigestValue);
    if (existing) {
      if (existing.runner) existing.runner.close(new Error("artifact replaced"));
      else {
        this.globalQueued -= existing.queue.length;
        for (const item of existing.queue) item.reject(new Error("artifact replaced"));
        existing.queue.length = 0;
      }
    }
    this.artifacts.set(policyDigestValue, {
      digest: policyDigestValue,
      sourceDigest: sourceDigestValue,
      sourceBytes: descriptor.sourceBytes,
      timeoutMs: policy.timeoutMs,
      memoryBytes: policy.memoryBytes,
      stackBytes: policy.stackBytes,
      allowedOps: new Set(policy.ids),
      source,
      queue: [],
      runner: undefined,
      ready: undefined,
      served: 0,
    });
    return policyDigestValue;
  }

  unpin(digest) {
    const artifact = this.artifacts.get(digest);
    if (!artifact) return false;
    this.artifacts.delete(digest);
    if (artifact.runner) {
      // runner.close() rejects and de-counts every queued item itself.
      artifact.runner.close(new Error("artifact unpinned"));
    } else {
      this.globalQueued -= artifact.queue.length;
      for (const item of artifact.queue) item.reject(new Error("artifact unpinned"));
      artifact.queue.length = 0;
    }
    return true;
  }

  // The invoke handler installed on the BrowserNode: (policyDigest,
  // requestJson) → Promise<string>. Invocations for unpinned digests reject —
  // executable source is never accepted from a peer, only ever pinned from a
  // verified server descriptor.
  invoke(digest, request) {
    if (this.closed) return Promise.reject(new Error("worker function runtime is closed"));
    const artifact = this.artifacts.get(digest);
    if (!artifact) return Promise.reject(new Error("artifact is not pinned locally"));
    if (typeof request !== "string") return Promise.reject(new Error("invoke request must be a JSON string"));
    if (artifact.queue.length + Number(artifact.runner?.busy === true) >= this.maxQueuePerArtifact) {
      return Promise.reject(new Error("function invocation queue is full"));
    }
    if (this.globalQueued >= this.maxQueuedGlobal) {
      return Promise.reject(new Error("global function invocation queue is full"));
    }
    this.globalQueued += 1;
    return new Promise((resolve, reject) => {
      artifact.queue.push({
        request,
        resolve: value => {
          artifact.served += 1;
          this.servedTotal += 1;
          resolve(value);
        },
        reject,
      });
      this.ensureRunner(artifact);
    });
  }

  ensureRunner(artifact) {
    if (!artifact.runner) {
      artifact.runner = new InlineFunctionRunner(this, artifact);
      artifact.ready = artifact.runner.boot();
    }
    artifact.ready.then(
      () => artifact.runner?.pump(),
      () => {
        /* the boot failure already closed the runner and rejected its queue */
      },
    );
  }

  // Global caps: active = invocations handed to a runner and not settled (one
  // per artifact at most, several across artifacts); queued = items waiting in
  // every per-artifact queue.
  beginInvoke() {
    if (this.globalActive >= this.maxActiveGlobal) return false;
    this.globalActive += 1;
    return true;
  }

  endInvoke(runner) {
    // runner.busy flips independently; only count an invocation this runner
    // actually had in flight.
    if (runner.busy) {
      runner.busy = false;
      this.globalActive -= 1;
      this.pumpAll();
    }
  }

  pumpAll() {
    for (const artifact of this.artifacts.values()) {
      if (artifact.queue.length > 0 && artifact.runner && !artifact.runner.closed) artifact.runner.pump();
    }
  }

  beginOp() {
    if (this.activeOps >= this.maxActiveOps) return false;
    this.activeOps += 1;
    return true;
  }

  endOp() {
    this.activeOps -= 1;
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
      for (const [runner, items] of byRunner) {
        if (runner.port) runner.port.postMessage({ kind: "opBatch", items });
      }
    });
  }

  stats() {
    return {
      pinned: [...this.artifacts.keys()],
      globalActive: this.globalActive,
      globalQueued: this.globalQueued,
      servedTotal: this.servedTotal,
      closed: this.closed,
    };
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    this.workerSourceController?.abort(new Error("worker function runtime closed"));
    for (const artifact of this.artifacts.values()) {
      artifact.runner?.close();
      for (const item of artifact.queue) item.reject(new Error("worker function runtime closed"));
      artifact.queue.length = 0;
    }
    this.globalQueued = 0;
    this.artifacts.clear();
  }
}
