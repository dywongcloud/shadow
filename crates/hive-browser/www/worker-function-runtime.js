// Worker-native bounded function runtime on the node-worker substrate
// (browser-worker-quickjs-runtime → bn-node-worker-substrate /
// execution-path-swap-off-quickjs).
// The DOM-only BrowserFunctionRuntime (function-runtime.js/function-runner.js)
// needs `document`, an iframe and page message events, so it cannot run inside
// the "Run a node" SharedWorker. This runtime is the worker-context lane:
//
//   * THE SUBSTRATE IS VENDORED node-worker (vendor/node-worker, Node's own lib
//     transpiled for a Worker), no longer QuickJS. An artifact runs in a
//     dedicated node-worker Worker realm of its own with the REAL Node module
//     set, so nothing needs the hand-written node-runtime.js shim any more —
//     the QuickJS guest's bare ECMAScript global (no console, no TextEncoder,
//     no URL, no timers) is not what we execute against.
//   * BOTH WIRE MODES RUN. `mode` is part of the canonical policy encoding the
//     server signs (`fluid_core::browser_policy_digest`), so the descriptor's
//     mode is still verified and still accepted — but under this substrate both
//     `quickjs` and `native` execute the same way, in the artifact's own
//     Worker, never in this trusted worker's context. (Under QuickJS `native`
//     meant host-context `eval`, which is why it was rejected here.)
//   * THE HOST IS A PAGE, BROKERED. node-worker's filesystem host needs
//     `navigator.serviceWorker` (`[Exposed=Window]`) and a `Worker`
//     constructor — a SharedWorker global scope has NEITHER — so this runtime
//     never creates the substrate itself: the caller injects `acquireHost`,
//     which brokers one from a connected page (node-worker-agent.js) and
//     returns a MessagePort-shaped end. Absent, every boot rejects with a
//     named `node_worker_*` reason rather than pretending to run.
//   * Execution budgets are capped by maxExecBlockMs BELOW the policy ceiling:
//     the deadline is the substrate's only bound (node-worker enforces no
//     memory or stack quota), and a guest that outlives it is terminated by
//     the host, so a shorter ceiling is a cheaper failure.
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

// Grace on top of the guest's deadline for the host to notice and terminate.
const SUBSTRATE_DEADLINE_GRACE_MS = 50;
const ARTIFACT_SOURCE_MAX_BYTES = 512 * 1024; // build contract caps entries at 256 KiB + fixed envelope
const DEFAULT_ACQUIRE_TIMEOUT_MS = 20_000; // a page must fetch and boot a multi-MB worker

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

/**
 * The guest's Wisp relay config (bn-node-worker-wisp-net), read from the
 * globals run-node-worker.js published out of the admission capability's `net`
 * block: the platform relay first (`HIVE_BROWSER_WISP_URL` on the fleet), then
 * the third-party public fallback, which travels as a PERMIT plus an address
 * rather than as a URL — the substrate proves that relay answers before it
 * routes anything at it, and says so when it does.
 *
 * `undefined` when there is nothing to send, which is the honest shape for a
 * fleet that configured no relay: the guest's first socket then throws the
 * substrate's named `WispRelayUnavailable` instead of resolving to nothing.
 */
function substrateNetConfig() {
  const url = wispUrlOrNull(globalThis.HIVE_WISP_URL);
  const fallbackUrl = wispUrlOrNull(globalThis.HIVE_WISP_PUBLIC_FALLBACK_URL);
  const allowFallback = globalThis.HIVE_WISP_PUBLIC_FALLBACK === true;
  if (!url && !fallbackUrl && !allowFallback) return undefined;
  const net = {};
  if (url) net.wispUrl = url;
  if (fallbackUrl) net.publicFallbackUrl = fallbackUrl;
  if (allowFallback) net.allowPublicFallback = true;
  return net;
}

function wispUrlOrNull(value) {
  return typeof value === "string" && /^wss?:\/\/[^\s]+$/.test(value.trim())
    ? value.trim()
    : null;
}

class SubstrateFunctionRunner {
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
      // The substrate host: a page-brokered node-worker Worker (see the file
      // header). `acquireHost` is the caller's broker; without it there is no
      // substrate at all, and the named error is the point of use.
      const acquire = this.runtime.acquireHost;
      if (typeof acquire !== "function") {
        throw new Error(
          "node_worker_no_host: the worker function runtime requires an `acquireHost` broker — " +
            "the node-worker substrate must be hosted by a page (a SharedWorker has neither a Worker constructor nor navigator.serviceWorker)",
        );
      }
      // A broker that never answers must not hold the boot open forever; the
      // timer is cleared either way so a settled acquire leaves nothing behind.
      let brokerTimer;
      const port = await Promise.race([
        Promise.resolve(acquire()),
        new Promise((_, reject) => {
          brokerTimer = setTimeout(
            () => reject(new Error(`node_worker_broker_timeout: no page supplied a node-worker host within ${DEFAULT_ACQUIRE_TIMEOUT_MS}ms`)),
            DEFAULT_ACQUIRE_TIMEOUT_MS,
          );
        }),
      ]).finally(() => clearTimeout(brokerTimer));
      if (controller.signal.aborted) throw abortError(controller.signal);
      this.port = port;
      this.port.onmessage = ({ data }) => this.onMessage(data);
      const booted = abortable(new Promise(resolve => {
        this.bootResolve = resolve;
      }), controller.signal);
      // The artifact is handed over as the EXACT verified bytes: pin()
      // size-checked, BLAKE3-matched and policy-digest-verified them, and the
      // host wraps `module.exports = (…)` AROUND them rather than editing
      // them. The runtime is substrate, exactly like `ops`: it grants no
      // capability the artifact's `allowed_ops` did not already grant.
      this.port.postMessage({
        kind: "boot",
        source: this.artifact.source,
        mode: this.artifact.mode,
        timeoutMs: this.artifact.timeoutMs,
        // bn-node-worker-wisp-net: the guest's `node:net` / `node:tls` config
        // travels WITH the boot message rather than being re-read by the host,
        // because the host runs in a PAGE realm that shares no global with this
        // worker — the platform relay lives in `globalThis.HIVE_WISP_URL` here
        // (published by run-node-worker.js from the admission capability's
        // `net` block) and would be invisible over there. Undefined means
        // "resolve it yourself", which for a page with no globals of its own
        // ends in the substrate's named `WispRelayUnavailable` at the first
        // socket — not in a silently network-less guest.
        net: substrateNetConfig(),
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
    // The budget actually handed to the substrate: the policy timeout clamped
    // to the exec ceiling. The guest now runs in its OWN worker, so this is
    // not a relay-liveness bound any more — it is a cost bound, and the only
    // one the substrate has (node-worker meters neither memory nor stack). The
    // host terminates a guest that outlives it; the timer below is the
    // backstop for an overrun, and an uninterruptible native call inside the
    // guest can still overshoot it.
    const deadlineMs = Math.min(this.artifact.timeoutMs, this.runtime.maxExecBlockMs);
    const timer = setTimeout(() => {
      const current = this.pending.get(id);
      if (!current) return;
      const error = new Error("function invocation timed out");
      current.controller.abort(error);
      current.reject(error);
      this.pending.delete(id);
      this.close(new Error("function runner terminated after timeout"));
    }, deadlineMs + SUBSTRATE_DEADLINE_GRACE_MS);
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
    // `acquireHost` is the caller's broker for a page-hosted node-worker
    // substrate (see the file header): a SharedWorker can neither construct a
    // Worker nor register a service worker, so the substrate is always
    // brokered. It is REQUIRED, and its absence is a named error at boot
    // rather than a lane that silently serves nothing.
    this.acquireHost = options.acquireHost;
    this.bootTimeoutMs = positiveInteger(options.bootTimeoutMs, 10000, "bootTimeoutMs");
    this.maxQueuePerArtifact = positiveInteger(options.maxQueuePerArtifact, 32, "maxQueuePerArtifact");
    this.maxQueuedGlobal = positiveInteger(options.maxQueuedGlobal, 64, "maxQueuedGlobal");
    this.maxActiveGlobal = positiveInteger(options.maxActiveGlobal, 4, "maxActiveGlobal");
    this.maxActiveOps = positiveInteger(options.maxActiveOps, 32, "maxActiveOps");
    // Ceiling on ONE invocation's wall-clock budget. Defaults to the
    // platform's own default policy timeout. The guest runs in its own worker
    // now, so this is no longer a relay-liveness bound — it is a cost bound,
    // and the substrate's only one (node-worker meters neither memory nor
    // stack), so a guest past it is terminated.
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
  //      platform registry, mode is one the wire contract defines (it is part
  //      of the canonical policy encoding the server signed, so it is verified
  //      here and never rewritten — under node-worker both modes execute in
  //      the artifact's own Worker realm, never in this trusted worker's);
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
    // `mode` still has to be one of the two wire values: it is a field of the
    // canonical policy encoding, so an unknown one is a descriptor the fleet
    // could not have signed. Both accepted values now run the same way.
    if (descriptor.mode !== "quickjs" && descriptor.mode !== "native") {
      throw new Error(`artifact mode ${JSON.stringify(descriptor.mode)} is unsupported — the wire contract defines quickjs and native`);
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
      // The descriptor's mode, carried verbatim: it is verified wire data, and
      // the substrate no longer branches on it.
      mode: policy.mode,
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
      artifact.runner = new SubstrateFunctionRunner(this, artifact);
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
    for (const artifact of this.artifacts.values()) {
      artifact.runner?.close();
      for (const item of artifact.queue) item.reject(new Error("worker function runtime closed"));
      artifact.queue.length = 0;
    }
    this.globalQueued = 0;
    this.artifacts.clear();
  }
}
