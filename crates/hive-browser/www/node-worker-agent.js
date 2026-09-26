// Page half of the node-worker execution lane
// (bn-node-worker-substrate / execution-path-swap-off-quickjs).
//
// WHY A PAGE HALF AT ALL. The node-worker substrate's filesystem host must be a
// page — `navigator.serviceWorker` is `[Exposed=Window]` and a SharedWorker
// global scope has no `Worker` constructor either — but the artifact, its
// policy and the host-op handlers all live in the run-node worker, which owns
// the node. So the worker asks a connected page to HOST the substrate and
// bridge a MessageChannel to it: the sqlite-worker and peer-mesh-agent broker
// pattern, verbatim.
//
// This file is deliberately the thinnest possible thing. It boots one
// NodeWorkerHost per artifact and translates one protocol:
//
//   worker -> page : { kind: "boot", source, mode?, timeoutMs? }
//   worker -> page : { kind: "invoke", id, request, deadlineMs }
//   worker -> page : { kind: "opBatch", items: [{ call, ok, value? | error? }] }
//   worker -> page : { kind: "close" }
//   page -> worker : { kind: "ready" }
//   page -> worker : { kind: "result", id, ok, value? | error? }
//   page -> worker : { kind: "op", id, call, op, payload }
//   page -> worker : { kind: "fatal", error }
//
// THE PAGE IS NOT TRUSTED WITH POLICY, exactly as in the mesh agent. It never
// decides whether an op is allowed — the worker does, against the artifact's
// own `allowed_ops`, and this side only carries the question and the answer. It
// does learn the artifact's source bytes (they have to be written into the
// guest filesystem), which is the minimum the substrate requires and the same
// bytes a donor's own browser is executing.
//
// NOT HOSTING IS A FULLY SUPPORTED STATE: an unbuilt lane, a browser with no
// service worker, a blocked module import — every one is reported as
// `{ kind: "fatal" }` with the named `node_worker_<reason>` message, the
// worker's runner closes, and the invocation rejects with that reason. Nothing
// here ever falls back to a silent no-op.

import { NodeWorkerHost, assetUrls } from "./node-worker-host.js";

const OP_RELAY_TIMEOUT_MS = 30_000;
/**
 * How long an agent waits for its `boot` before giving up on itself.
 *
 * The worker posts `boot` immediately after it receives the transferred port,
 * so an agent that never sees one is an orphan: its requester timed out, or
 * the runner closed without a `close`. An orphan holds a real node-worker
 * Worker (and a service-worker session) alive for nobody.
 */
const UNBOOTED_TTL_MS = 30_000;

/** Start one agent bound to `port` (the worker's end of a MessageChannel).
 *  Returns { stop() } — call it on `{ kind: "close" }`, on `nodeWorkerDone`
 *  and on page unload; an orphaned host holds a real Worker alive. */
export function startNodeWorkerAgent(port) {
  let host = null;
  let currentId = 0;
  let nextCall = 1;
  let stopped = false;
  const pendingOps = new Map();
  const urls = assetUrls("./node-worker/");
  const unbooted = setTimeout(() => stop(), UNBOOTED_TTL_MS);

  const send = message => {
    if (stopped) return;
    try {
      port.postMessage(message);
    } catch {
      /* worker gone; stop() follows from its own teardown */
    }
  };

  const fail = error => {
    const message = String(error?.message || error);
    send({ kind: "fatal", error: message });
    if (!message.startsWith("node_worker_")) return;
    stop();
  };

  // One host op, relayed to the worker: the guest is PARKED in
  // `chan.callSync` while this is in flight, so the answer must arrive or the
  // call must reject — an op that is never answered hangs the artifact until
  // its own deadline.
  function relayOp(op, payload) {
    if (!Number.isSafeInteger(op) || op < 0) return Promise.resolve({ ok: false, error: `operation ${op} is not a valid op id` });
    const call = nextCall++;
    return new Promise(resolve => {
      const timer = setTimeout(() => {
        pendingOps.delete(call);
        resolve({ ok: false, error: `host operation ${op} was not answered within ${OP_RELAY_TIMEOUT_MS}ms` });
      }, OP_RELAY_TIMEOUT_MS);
      pendingOps.set(call, result => {
        clearTimeout(timer);
        resolve(result);
      });
      send({ kind: "op", id: currentId, call, op, payload });
    });
  }

  async function boot(message) {
    clearTimeout(unbooted);
    stopHost();
    host = await NodeWorkerHost.boot({
      indexUrl: message?.urls?.indexUrl || urls.indexUrl,
      workerUrl: message?.urls?.workerUrl || urls.workerUrl,
      swUrl: message?.urls?.swUrl || urls.swUrl,
      artifactSource: message?.source,
      timeoutMs: message?.timeoutMs,
      bootTimeoutMs: message?.bootTimeoutMs,
      // bn-node-worker-wisp-net: the relay config arrived in the boot message
      // rather than in a global, because this page realm shares none with the
      // SharedWorker that read the admission capability.
      net: message?.net,
      onOp: ({ op, payload }) => relayOp(op, payload),
    });
    if (stopped) return;
    send({ kind: "ready" });
  }

  async function invoke(message) {
    const id = Number(message?.id);
    if (!Number.isSafeInteger(id) || id < 0) {
      send({ kind: "result", id: message?.id, ok: false, error: "invoke messages must carry a non-negative integer id" });
      return;
    }
    if (!host) {
      send({ kind: "result", id, ok: false, error: "node_worker_not_booted: the substrate is not running" });
      return;
    }
    currentId = id;
    try {
      const value = await host.invoke(String(message?.request ?? ""), message?.deadlineMs);
      send({ kind: "result", id, ok: true, value });
    } catch (error) {
      send({ kind: "result", id, ok: false, error: String(error?.message || error) });
    }
  }

  function complete(items) {
    if (!Array.isArray(items)) return;
    for (const item of items) {
      if (!item || !Number.isSafeInteger(item.call)) continue;
      const settle = pendingOps.get(item.call);
      if (!settle) continue;
      pendingOps.delete(item.call);
      settle(item.ok === true ? { ok: true, value: item.value } : { ok: false, error: String(item.error || "host operation failed") });
    }
  }

  function stopHost() {
    const current = host;
    host = null;
    for (const settle of pendingOps.values()) settle({ ok: false, error: "the node-worker host stopped" });
    pendingOps.clear();
    try {
      current?.close();
    } catch {
      /* already closed */
    }
  }

  function stop() {
    if (stopped) return;
    stopped = true;
    clearTimeout(unbooted);
    stopHost();
    try {
      port.close();
    } catch {
      /* already closed */
    }
  }

  port.onmessage = event => {
    const message = event?.data;
    if (stopped || !message || typeof message !== "object") return;
    if (message.kind === "boot") void boot(message).catch(fail);
    else if (message.kind === "invoke") void invoke(message);
    else if (message.kind === "opBatch") complete(message.items);
    else if (message.kind === "close") stop();
  };
  if (typeof port.start === "function") port.start();

  return { stop };
}
