const QUICKJS_INTERRUPT_GRACE_MS = 50;

function transferables(value, out = []) {
  if (value instanceof ArrayBuffer) out.push(value);
  else if (ArrayBuffer.isView(value)) out.push(value.buffer);
  else if (value && typeof value === "object") {
    for (const item of Object.values(value)) transferables(item, out);
  }
  return [...new Set(out)];
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
    const ready = new Promise((resolve, reject) => {
      const onMessage = event => {
        if (event.source !== this.frame.contentWindow || event.data?.kind !== "frameReady") return;
        clearTimeout(timer);
        removeEventListener("message", onMessage);
        resolve(event.data.nonce);
      };
      const timer = setTimeout(() => {
        removeEventListener("message", onMessage);
        reject(new Error("function frame boot timed out"));
      }, this.runtime.bootTimeoutMs);
      addEventListener("message", onMessage);
    });
    document.body.append(this.frame);
    const nonce = await ready;
    if (this.closed) throw new Error("runner closed during boot");

    const channel = new MessageChannel();
    this.port = channel.port1;
    this.port.onmessage = event => this.onMessage(event.data);
    this.port.start();
    const workerSource = await this.runtime.workerSource;
    const booted = new Promise((resolve, reject) => {
      this.bootResolve = resolve;
      this.bootReject = reject;
    });
    this.frame.contentWindow.postMessage({
      kind: "boot",
      nonce,
      workerSource,
      artifact: this.artifact.workerConfig,
    }, "*", [channel.port2]);
    await booted;
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
    const timeoutGrace = this.artifact.workerConfig.mode === "quickjs"
      ? QUICKJS_INTERRUPT_GRACE_MS
      : 0;
    const timer = setTimeout(() => {
      item.reject(new Error("function invocation timed out"));
      this.pending.delete(id);
      this.close(new Error("function runner terminated after timeout"));
    }, this.artifact.timeoutMs + timeoutGrace);
    this.pending.set(id, { ...item, timer });
    this.port.postMessage({ kind: "invoke", id, request: item.request, deadlineMs: this.artifact.timeoutMs });
  }

  onMessage(message) {
    if (this.closed) return;
    if (message.kind === "ready") {
      this.bootResolve?.();
      this.bootResolve = undefined;
      this.bootReject = undefined;
    } else if (message.kind === "fatal") {
      const error = new Error(`function worker failed: ${message.error}`);
      this.bootReject?.(error);
      this.close(error);
    } else if (message.kind === "result") {
      const item = this.pending.get(message.id);
      if (!item) return;
      clearTimeout(item.timer);
      this.pending.delete(message.id);
      this.busy = false;
      if (message.ok) item.resolve(message.value);
      else item.reject(new Error(message.error));
      this.pump();
    } else if (message.kind === "op") {
      this.runOp(message);
    }
  }

  async runOp(message) {
    const op = this.runtime.ops.get(message.op);
    const allowed = this.artifact.allowedOps.has(message.op);
    let result;
    if (!allowed) result = { call: message.call, ok: false, error: `operation ${message.op} is denied` };
    else if (!op) result = { call: message.call, ok: false, error: `operation ${message.op} is unavailable` };
    else {
      try {
        result = {
          call: message.call,
          ok: true,
          value: await op(message.payload, { digest: this.artifact.digest }),
        };
      } catch (error) {
        result = { call: message.call, ok: false, error: String(error?.message || error) };
      }
    }
    if (this.closed) return;
    this.runtime.opCompletions.push({ runner: this, result });
    this.runtime.scheduleOpFlush();
  }

  close(error = new Error("function runner closed")) {
    if (this.closed) return;
    this.closed = true;
    this.port?.postMessage({ kind: "close" });
    this.port?.close();
    this.frame.remove();
    this.bootReject?.(error);
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
