import {
  RELEASE_SYNC,
  newQuickJSWASMModule,
  newVariant,
} from "quickjs-emscripten";
import wasmBytes from "@jitl/quickjs-wasmfile-release-sync/wasm";

const variant = newVariant(RELEASE_SYNC, { wasmBinary: wasmBytes });

function bridgeValue(value) {
  if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) {
    const bytes = value instanceof ArrayBuffer
      ? new Uint8Array(value)
      : new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    let binary = "";
    for (let offset = 0; offset < bytes.length; offset += 0x8000) {
      binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
    }
    return { $bytes: btoa(binary) };
  }
  if (Array.isArray(value)) return value.map(bridgeValue);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, bridgeValue(item)]));
  }
  return value;
}

export async function createQuickRunner(source, limits, post) {
  const QuickJS = await newQuickJSWASMModule(variant);
  const runtime = QuickJS.newRuntime();
  runtime.setMemoryLimit(limits.memoryBytes);
  runtime.setMaxStackSize(limits.stackBytes);

  const vm = runtime.newContext();
  const pending = new Map();
  let active;
  let invokingId;
  let deadline = Number.MAX_SAFE_INTEGER;
  let nextOp = 1;
  runtime.setInterruptHandler(() => Date.now() > deadline);

  const hostOp = vm.newFunction("__hive_host_op", (opHandle, payloadHandle) => {
    const invocationId = active?.id ?? invokingId;
    if (invocationId === undefined) throw new Error("host operation outside invocation");
    const op = vm.getNumber(opHandle);
    const payload = JSON.parse(vm.getString(payloadHandle));
    const call = nextOp++;
    const deferred = vm.newPromise();
    pending.set(call, deferred);
    post({ kind: "op", id: invocationId, call, op, payload });
    return deferred.handle;
  });
  vm.setProp(vm.global, "__hive_host_op", hostOp);
  hostOp.dispose();

  const installed = vm.evalCode(`
    globalThis.__hive_base64 = text => {
      if (text.length % 4 !== 0) throw new TypeError("invalid base64 host value");
      const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
      const padding = text.endsWith("==") ? 2 : text.endsWith("=") ? 1 : 0;
      const out = new Uint8Array(text.length / 4 * 3 - padding);
      let written = 0;
      for (let offset = 0; offset < text.length; offset += 4) {
        const a = alphabet.indexOf(text[offset]);
        const b = alphabet.indexOf(text[offset + 1]);
        const c = text[offset + 2] === "=" ? 0 : alphabet.indexOf(text[offset + 2]);
        const d = text[offset + 3] === "=" ? 0 : alphabet.indexOf(text[offset + 3]);
        if (a < 0 || b < 0 || c < 0 || d < 0) throw new TypeError("invalid base64 host value");
        const bits = a << 18 | b << 12 | c << 6 | d;
        if (written < out.length) out[written++] = bits >>> 16 & 255;
        if (written < out.length) out[written++] = bits >>> 8 & 255;
        if (written < out.length) out[written++] = bits & 255;
      }
      return out;
    };
    globalThis.__hive_decode = value => {
      if (value && typeof value === "object" && typeof value.$bytes === "string") {
        return __hive_base64(value.$bytes);
      }
      if (Array.isArray(value)) return value.map(__hive_decode);
      if (value && typeof value === "object") {
        for (const key of Object.keys(value)) value[key] = __hive_decode(value[key]);
      }
      return value;
    };
    globalThis.__hive_ops = Object.freeze({
      call: (op, payload) => __hive_host_op(op, JSON.stringify(payload)).then(
        value => __hive_decode(JSON.parse(value))
      )
    });
    globalThis.__hive_handler = (${source});
    if (typeof __hive_handler !== "function") throw new TypeError("artifact must evaluate to a function");
    globalThis.__hive_invoke = request => Promise.resolve(__hive_handler(request, __hive_ops));
  `, "artifact.js");
  if (installed.error) {
    const error = vm.dump(installed.error);
    installed.error.dispose();
    vm.dispose();
    runtime.dispose();
    throw new Error(String(error?.message || error));
  }
  installed.value.dispose();

  function drain() {
    const jobs = runtime.executePendingJobs();
    if (jobs.error) {
      const error = jobs.error.context.dump(jobs.error);
      jobs.error.dispose();
      throw new Error(String(error?.message || error));
    }
  }

  function settleActive() {
    drain();
    const state = vm.getPromiseState(active.handle);
    if (state.type === "pending") {
      if (pending.size === 0 && !runtime.hasPendingJob()) {
        throw new Error("function left an unresolved promise without pending host operations");
      }
      return;
    }
    const current = active;
    active = undefined;
    if (state.type === "rejected") {
      const error = vm.dump(state.error);
      state.error.dispose();
      current.handle.dispose();
      current.reject(new Error(String(error?.message || error)));
      return;
    }
    const value = vm.dump(state.value);
    state.value.dispose();
    current.handle.dispose();
    if (typeof value !== "string") current.reject(new Error("function result must be a string"));
    else current.resolve(value);
  }

  return {
    invoke(id, request, deadlineMs) {
      if (active) return Promise.reject(new Error("QuickJS runner is already executing"));
      deadline = Date.now() + deadlineMs;
      const handler = vm.getProp(vm.global, "__hive_invoke");
      const requestHandle = vm.newString(request);
      let called;
      invokingId = id;
      try {
        called = vm.callFunction(handler, vm.undefined, requestHandle);
      } finally {
        invokingId = undefined;
        handler.dispose();
        requestHandle.dispose();
      }
      if (called.error) {
        const error = vm.dump(called.error);
        called.error.dispose();
        return Promise.reject(new Error(String(error?.message || error)));
      }
      return new Promise((resolve, reject) => {
        active = { id, handle: called.value, resolve, reject };
        try { settleActive(); } catch (error) {
          called.value.dispose();
          active = undefined;
          reject(error);
        }
      });
    },
    complete(items) {
      for (const item of items) {
        const deferred = pending.get(item.call);
        if (!deferred) continue;
        pending.delete(item.call);
        if (item.ok) {
          const value = vm.newString(JSON.stringify(bridgeValue(item.value)));
          deferred.resolve(value);
          value.dispose();
        } else {
          const error = vm.newError(item.error);
          deferred.reject(error);
          error.dispose();
        }
      }
      if (!active) return;
      try { settleActive(); } catch (error) {
        const current = active;
        active = undefined;
        current.handle.dispose();
        current.reject(error);
      }
    },
    close(error) {
      if (active) {
        active.handle.dispose();
        active.reject(error);
        active = undefined;
      }
      for (const deferred of pending.values()) deferred.dispose();
      pending.clear();
      vm.dispose();
      runtime.dispose();
    },
  };
}
