// "Run a node" SharedWorker — the trusted owner of the browser's iroh
// endpoint and its admission lifecycle (bn-ui-sharedworker-owner). Production
// capability closure: the live BrowserNode instance lives ONLY in this
// module's closure, never on `window`, and is reachable only through the
// versioned status/control message protocol below (mirrors
// ui/lib/run-node-status.ts's RunNodeStatus shape).
//
// Geolocation is intentionally NOT read here — the Geolocation API does not
// exist in a worker context. The page captures + quantizes-for-display
// consent on the main thread and posts coordinates in only via the
// `presence` control message; this worker never decides consent policy.

import init, { BrowserNode, wasmBundleVersion } from "./browser-node/pkg/hive_browser.js";
import { loadOrCreateSeed } from "./browser-node/identity.js";

const PROTOCOL_VERSION = 0; // must match hive_browser_proto::BROWSER_PROTOCOL_VERSION
const RENEW_INTERVAL_MS = 60_000; // inside the backend's [30s,300s] lease window
// bn-p2p-version-negotiation (host-operation ABI): must match
// ui/lib/run-node-status.ts's HOST_ABI_VERSION -- lets a page detect it's
// talking to a stale SharedWorker instance still running old code (which a
// plain page reload does NOT replace; every connecting tab has to close
// first). Fixed on the status object below, never patched, so it survives
// every `{...status, ...patch}` spread in setStatus() untouched.
const HOST_ABI_VERSION = 1;
// bn-p2p-version-negotiation (PWA wasm bundle): must match
// crates/hive-browser/src/lib.rs's wasm_bundle_version() return value --
// checked live against the ACTUAL loaded module right after init() succeeds
// (see start() below), not just trusted by filename/cache-key, so a stale
// service-worker-cached .wasm paired with fresh JS glue is caught even
// though the two are normally synced/cached together as a pair.
const WASM_BUNDLE_VERSION = 1;

const ports = [];
// Per-port visibility (bn-p2p-bfcache-lifecycle): a SharedWorker outlives any
// single tab's page-lifecycle state, so the CONNECTION stays up across a
// bfcache entry — only the status this worker reports changes, honestly
// reflecting whether any connected tab can currently promise foreground
// reliability rather than claiming "online" while every tab is hidden/frozen.
const portVisibility = new Map();
let node = null;
let renewTimer = null;
let closing = false;
let session = null; // { deployment, fn, digest, scope, team, relay }

let status = {
  version: 0,
  abiVersion: HOST_ABI_VERSION,
  lifecycle: "stopped",
  endpointId: null,
  relay: null,
  admission: "none",
  geoConsent: "undecided",
  protocolMismatch: "none",
  lastError: null,
  updatedMs: Date.now(),
};

// bn-p2p-version-negotiation: the backend prefixes its two protocol-mismatch
// rejections with a stable marker (see crates/hive-cloud/src/browser_admission.rs
// validate_request) so the two directions get distinct client treatment
// instead of both looking like a generic admission failure.
function classifyAdmissionError(message) {
  if (typeof message !== "string") return "none";
  if (message.startsWith("protocol_too_old")) return "outdated";
  if (message.startsWith("protocol_too_new")) return "server_upgrading";
  // Same remedy as "outdated" (a reload re-fetches the wasm bundle fresh) --
  // distinct root cause (a stale LOCAL cache, not the fleet's protocol
  // floor), same UI treatment either way.
  if (message.startsWith("wasm_bundle_stale")) return "outdated";
  return "none";
}

function broadcast() {
  status = { ...status, version: status.version + 1, updatedMs: Date.now() };
  for (const p of ports) {
    try {
      p.postMessage({ type: "status", status });
    } catch {
      /* a port whose page has gone away — harmless, pruned lazily below */
    }
  }
}

function setStatus(patch) {
  status = { ...status, ...patch };
  broadcast();
}

async function callApi(method, path, team, body) {
  const r = await fetch(`/cloud${path}`, {
    method,
    credentials: "same-origin",
    headers: { "content-type": "application/json", "x-hive-team": team },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!r.ok) {
    const text = await r.text().catch(() => "");
    throw new Error(text || `${method} ${path} -> ${r.status}`);
  }
  return r.json();
}

async function admitOnce(addrJson, endpointId) {
  const { deployment, fn, digest, scope, team } = session;
  await callApi("POST", "/v1/browser/admissions", team, {
    endpoint_id: endpointId,
    addr_json: addrJson,
    deployment,
    function: fn,
    digest,
    scope: scope || "team",
    protocol_version: PROTOCOL_VERSION,
  });
}

function scheduleRenew(addrJson, endpointId) {
  if (renewTimer) clearInterval(renewTimer);
  renewTimer = setInterval(async () => {
    if (closing || !node) return;
    try {
      await admitOnce(addrJson, endpointId);
      // Derive lifecycle from CURRENT visibility rather than hardcoding
      // "online" — a background renewal tick must not un-suspend a node
      // whose tabs are all still hidden.
      const lifecycle = anyVisible() ? "online" : "suspended";
      if (status.admission !== "granted" || status.lifecycle !== lifecycle || status.protocolMismatch !== "none") {
        setStatus({ admission: "granted", lifecycle, protocolMismatch: "none", lastError: null });
      }
    } catch (e) {
      const message = String((e && e.message) || e);
      const mismatch = classifyAdmissionError(message);
      if (mismatch === "outdated") {
        // No retry will ever succeed against this server's floor — stop
        // spending renewal cycles on a doomed request and let the UI prompt
        // a reload instead of silently failing every 60s forever.
        clearInterval(renewTimer);
        renewTimer = null;
        setStatus({ admission: "denied", lifecycle: "error", protocolMismatch: mismatch, lastError: message });
        return;
      }
      // "server_upgrading" (transient) and any other failure keep retrying
      // on the same schedule — the next tick may hit an already-upgraded
      // node or a transient failure may simply clear.
      setStatus({ admission: "denied", lifecycle: "degraded", protocolMismatch: mismatch, lastError: message });
    }
  }, RENEW_INTERVAL_MS);
}

async function start(msg) {
  if (node || status.lifecycle === "starting") return;
  session = {
    deployment: msg.deployment,
    fn: msg.fn,
    digest: msg.digest,
    scope: msg.scope,
    team: msg.team,
    relay: msg.relay,
  };
  setStatus({ lifecycle: "starting", lastError: null, admission: "pending" });
  try {
    await init();
    // Check the ACTUAL loaded module's version, not just an assumption from
    // the URL/cache key — catches a stale service-worker-cached .wasm binary
    // paired with fresh JS glue, which a filename/URL check alone can't see.
    const loadedWasmVersion = wasmBundleVersion();
    if (loadedWasmVersion !== WASM_BUNDLE_VERSION) {
      throw new Error(
        `wasm_bundle_stale: loaded module reports version ${loadedWasmVersion}, this worker expects ${WASM_BUNDLE_VERSION} — reload to fetch the current bundle`,
      );
    }
    // Identity persistence (bn-impl-key-persistence): boot a throwaway node
    // ONLY to obtain a fresh seed if none is persisted yet, then re-boot from
    // the persisted seed so the reported id is stable across reloads.
    const scratch = await BrowserNode.boot(msg.relay, null, null);
    const { seedHex, created } = await loadOrCreateSeed(scratch.secretHex());
    let n;
    if (created) {
      n = scratch;
    } else {
      await scratch.close();
      scratch.free();
      n = await BrowserNode.boot(msg.relay, msg.discovery || null, seedHex);
    }
    node = n;
    const endpointId = node.nodeId();
    const addrJson = node.addrJson();
    setStatus({ endpointId, relay: msg.relay, lifecycle: anyVisible() ? "online" : "suspended" });
    await admitOnce(addrJson, endpointId);
    setStatus({ admission: "granted", protocolMismatch: "none", lastError: null });
    scheduleRenew(addrJson, endpointId);
  } catch (e) {
    const message = String((e && e.message) || e);
    setStatus({
      lifecycle: "error",
      admission: "denied",
      protocolMismatch: classifyAdmissionError(message),
      lastError: message,
    });
    node = null;
  }
}

function anyVisible() {
  if (portVisibility.size === 0) return true; // no reports yet — assume foreground
  for (const visible of portVisibility.values()) if (visible) return true;
  return false;
}

function onPortVisibility(port, msg) {
  if (msg.unloading) {
    portVisibility.delete(port);
    const idx = ports.indexOf(port);
    if (idx !== -1) ports.splice(idx, 1);
    // No tab left that can observe or control this node — release it rather
    // than run headless forever with no way for a user to ever stop it.
    if (ports.length === 0 && (node || status.lifecycle === "starting")) {
      stop();
    }
    return;
  }
  portVisibility.set(port, !!msg.visible);
  // Only toggle between online/suspended — never override starting/error/
  // stopped, which are driven by the connect/admit lifecycle itself.
  if (status.lifecycle === "online" && !anyVisible()) {
    setStatus({ lifecycle: "suspended" });
  } else if (status.lifecycle === "suspended" && anyVisible()) {
    setStatus({ lifecycle: "online" });
  }
}

async function stop() {
  closing = true;
  if (renewTimer) clearInterval(renewTimer);
  renewTimer = null;
  const endpointId = status.endpointId;
  const team = session && session.team;
  if (node) {
    try {
      if (endpointId && team) {
        await callApi("DELETE", `/v1/browser/admissions/${encodeURIComponent(endpointId)}`, team);
      }
    } catch {
      /* best-effort revoke — the lease also expires on its own */
    }
    try {
      await node.close();
    } catch {
      /* already gone */
    }
    try {
      node.free();
    } catch {
      /* already gone */
    }
    node = null;
  }
  session = null;
  closing = false;
  setStatus({
    lifecycle: "stopped",
    admission: "none",
    protocolMismatch: "none",
    endpointId: null,
    relay: null,
    lastError: null,
  });
}

// Shared by both execution contexts below: a SharedWorker hands this ONE real
// MessagePort per connecting tab; a plain dedicated Worker (the Web Locks
// fallback, bn-ui-sharedworker-owner) has no ports at all, only `self`'s own
// postMessage/onmessage — but `self` in a DedicatedWorkerGlobalScope already
// shapes-match everything this function actually calls (postMessage,
// assignable onmessage), so passing `self` as `port` works unchanged and this
// is the ENTIRE production BrowserNode-owning logic in both contexts, not a
// reimplementation for the fallback path.
function connectPort(port) {
  ports.push(port);
  port.onmessage = (e) => {
    const msg = e.data || {};
    if (msg.type === "start") start(msg);
    else if (msg.type === "stop") stop();
    else if (msg.type === "geoConsent") setStatus({ geoConsent: msg.value });
    else if (msg.type === "visibility") onPortVisibility(port, msg);
    else if (msg.type === "status") {
      try {
        port.postMessage({ type: "status", status });
      } catch {
        /* ignore */
      }
    }
  };
  // MessagePort.start() has no equivalent (and no need for one) on a
  // DedicatedWorkerGlobalScope's `self` — messages already flow once
  // onmessage is assigned.
  if (typeof port.start === "function") port.start();
  try {
    port.postMessage({ type: "status", status });
  } catch {
    /* ignore */
  }
}

if (typeof SharedWorkerGlobalScope !== "undefined" && self instanceof SharedWorkerGlobalScope) {
  self.onconnect = (event) => connectPort(event.ports[0]);
} else {
  // Web Locks fallback (bn-ui-sharedworker-owner): this script was loaded as
  // a plain dedicated Worker by a tab that lost SharedWorker feature
  // detection (Safari private mode, an older engine, or an explicit
  // capability failure) but won the navigator.locks single-owner election in
  // ui/lib/use-run-node.ts. Exactly one "port" exists — `self` itself — so
  // there is only ever one connectPort() call here, never a growing `ports`
  // array the way a real SharedWorker accumulates one per tab.
  connectPort(self);
}
