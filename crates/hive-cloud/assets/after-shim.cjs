// hive after()/waitUntil runtime support — injected into Node-runtime deployments
// via NODE_OPTIONS=--require. No-op unless the app actually uses after()/waitUntil.
//
// Next.js `after()` (and any code that wants a platform keep-alive) looks up a
// platform waitUntil at:
//     globalThis[Symbol.for('@next/request-context')].get().waitUntil
// Next accesses it AT after() call-time (during request handling, before the
// response headers flush), per the Next.js docs "supporting after for serverless
// platforms". This shim provides that primitive backed by an AsyncLocalStorage
// scoped per request, and signals the platform to keep the instance warm by
// emitting the `x-fluid-wait-until-ms` response header — the exact keep-alive
// convention the fluid tunnel/gateway already honor (the header is read + stripped
// guest-side and the gateway holds the instance lease that long after the response
// drains, so background work isn't torn down).
'use strict';
try {
  const http = require('http');
  const { AsyncLocalStorage } = require('async_hooks');
  const als = new AsyncLocalStorage();
  const SYM = Symbol.for('@next/request-context');

  // Keep-alive window (ms) the platform grants for background work — the
  // function's configured maxDuration (HIVE_AFTER_MAX_MS), default 300s.
  const MAX_MS = Math.max(0, parseInt(process.env.HIVE_AFTER_MAX_MS || '300000', 10) || 300000);

  // Next.js reads `.get().waitUntil` off this global. Only install if absent so a
  // real platform adapter (Vercel) that set it first always wins.
  if (!globalThis[SYM]) {
    globalThis[SYM] = { get: () => als.getStore() };
  }

  // Run every incoming request inside an ALS store carrying a waitUntil bound to
  // THIS response. Patch http.Server.prototype.emit at the prototype level before
  // the app (Next) creates its server, so every server is covered. AsyncLocalStorage
  // propagates the store through the async request lifecycle started within emit.
  const origEmit = http.Server.prototype.emit;
  http.Server.prototype.emit = function emit(type) {
    if (type !== 'request') return origEmit.apply(this, arguments);
    const res = arguments[2];
    let declared = false;
    const store = {
      // Next's after()/waitUntil funnels its drain promise here. We don't need to
      // await it — the platform holds the lease for the fixed window. We DO need to
      // declare that window before headers flush (which is when Next calls this).
      waitUntil(promise) {
        try { if (promise && typeof promise.then === 'function') promise.then(undefined, () => {}); } catch (e) {}
        if (!declared && res && !res.headersSent && MAX_MS > 0) {
          try { res.setHeader('x-fluid-wait-until-ms', String(MAX_MS)); declared = true; } catch (e) {}
        }
      },
    };
    return als.run(store, () => origEmit.apply(this, arguments));
  };
} catch (e) {
  // Never break a deployment because of the shim.
}
