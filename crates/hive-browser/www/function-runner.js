const QUICKJS_INTERRUPT_GRACE_MS = 50;

function transferables(value, out = []) {
  if (value instanceof ArrayBuffer) out.push(value);
  else if (ArrayBuffer.isView(value)) out.push(value.buffer);
  else if (value && typeof value === "object") {
    for (const item of Object.values(value)) transferables(item, out);
  }
  return [...new Set(out)];
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

function validOperationId(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

export function post(target, message) {
  target.postMessage(message, { transfer: transferables(message) });
}

export class FunctionRunner {
  constructor(runtime, artifact) {
    this.runtime = runtime;
    this.artifact = artifact;
    this.frame = document.createElement("iframe");
    this.frame.hidden = true;
    this.frame.sandbox = "allow-scripts";
    this.frame.src = runtime.frameUrl;
    this.pending = new Map();
    this.queue = [];
    this.nextId = 1;
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
      const ready = new Promise(resolve => {
        const onMessage = event => {
          if (event.source !== this.frame.contentWindow || event.data?.kind !== "frameReady") return;
          removeEventListener("message", onMessage);
          resolve(event.data.nonce);
        };
        this.frameReadyCleanup = () => removeEventListener("message", onMessage);
        addEventListener("message", onMessage);
      });
      document.body.append(this.frame);
      const nonce = await abortable(ready, controller.signal);
      this.frameReadyCleanup?.();
      this.frameReadyCleanup = undefined;

      const workerSource = await this.runtime.loadWorkerSource(controller.signal);
      if (controller.signal.aborted) throw abortError(controller.signal);
      const channel = new MessageChannel();
      this.port = channel.port1;
      this.port.onmessage = event => this.onMessage(event.data);
      this.port.start();
      const booted = abortable(new Promise(resolve => {
        this.bootResolve = resolve;
      }), controller.signal);
      this.frame.contentWindow.postMessage({
        kind: "boot",
        nonce,
        workerSource,
        artifact: this.artifact.workerConfig,
      }, "*", [channel.port2]);
      await booted;
    } finally {
      clearTimeout(timer);
      this.frameReadyCleanup?.();
      this.frameReadyCleanup = undefined;
      this.bootResolve = undefined;
      if (this.bootController === controller) this.bootController = undefined;
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
    if (this.closed || this.busy || this.queue.length === 0) return;
    this.busy = true;
    const item = this.queue.shift();
    const id = this.nextId++;
    const controller = new AbortController();
    const timeoutGrace = this.artifact.workerConfig.mode === "quickjs"
      ? QUICKJS_INTERRUPT_GRACE_MS
      : 0;
    const timer = setTimeout(() => {
      const current = this.pending.get(id);
      if (!current) return;
      const error = new Error("function invocation timed out");
      current.controller.abort(error);
      current.reject(error);
      this.pending.delete(id);
      this.close(new Error("function runner terminated after timeout"));
    }, this.artifact.timeoutMs + timeoutGrace);
    this.pending.set(id, { ...item, timer, controller, calls: new Set() });
    this.port.postMessage({ kind: "invoke", id, request: item.request, deadlineMs: this.artifact.timeoutMs });
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
      this.busy = false;
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
    const operation = this.artifact.ops.get(message.op);
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
          value: await operation.handler(message.payload, {
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
    this.bootController?.abort(error);
    this.frameReadyCleanup?.();
    this.port?.postMessage({ kind: "close" });
    this.port?.close();
    this.frame.remove();
    for (const item of this.pending.values()) {
      clearTimeout(item.timer);
      item.controller.abort(error);
      item.reject(error);
    }
    for (const item of this.queue) item.reject(error);
    this.pending.clear();
    this.queue.length = 0;
    this.runtime.runnerClosed(this.artifact, this);
  }
}
