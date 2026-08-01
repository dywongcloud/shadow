import { createQuickRunner } from "./quickjs-runner.js";

let runner;

function transferables(value, out = []) {
  if (value instanceof ArrayBuffer) out.push(value);
  else if (ArrayBuffer.isView(value)) out.push(value.buffer);
  else if (value && typeof value === "object") {
    for (const item of Object.values(value)) transferables(item, out);
  }
  return [...new Set(out)];
}

function post(message) {
  self.postMessage(message, { transfer: transferables(message) });
}

function createNativeRunner(source) {
  const handler = (0, eval)(`(${source})`);
  if (typeof handler !== "function") throw new Error("artifact must evaluate to a function");
  const pending = new Map();
  let nextOp = 1;

  return {
    async invoke(id, request) {
      const ops = {
        call(op, payload) {
          return new Promise((resolve, reject) => {
            const call = nextOp++;
            pending.set(call, { resolve, reject });
            post({ kind: "op", id, call, op, payload });
          });
        },
      };
      const value = await handler(request, ops);
      if (typeof value !== "string") throw new Error("function result must be a string");
      return value;
    },
    complete(items) {
      for (const item of items) {
        const deferred = pending.get(item.call);
        if (!deferred) continue;
        pending.delete(item.call);
        if (item.ok) deferred.resolve(item.value);
        else deferred.reject(new Error(item.error));
      }
    },
    close(error) {
      for (const deferred of pending.values()) deferred.reject(error);
      pending.clear();
    },
  };
}

self.onmessage = async ({ data }) => {
  try {
    if (data.kind === "boot") {
      if (runner) throw new Error("worker already booted");
      runner = data.mode === "quickjs"
        ? await createQuickRunner(data.source, data.limits, post)
        : createNativeRunner(data.source);
      post({ kind: "ready" });
      return;
    }
    if (data.kind === "invoke") {
      if (!runner) throw new Error("worker is not booted");
      try {
        const value = await runner.invoke(data.id, data.request, data.deadlineMs);
        post({ kind: "result", id: data.id, ok: true, value });
      } catch (error) {
        post({ kind: "result", id: data.id, ok: false, error: String(error?.message || error) });
      }
    } else if (data.kind === "opBatch") {
      runner?.complete(data.items);
    } else if (data.kind === "close") {
      runner?.close(new Error("runner closed"));
      self.close();
    }
  } catch (error) {
    post({ kind: "fatal", error: String(error?.message || error) });
  }
};
