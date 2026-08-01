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

import init, { BrowserNode } from "./browser-node/pkg/hive_browser.js";
import { loadOrCreateSeed } from "./browser-node/identity.js";

const PROTOCOL_VERSION = 0; // must match hive_browser_proto::BROWSER_PROTOCOL_VERSION
const RENEW_INTERVAL_MS = 60_000; // inside the backend's [30s,300s] lease window

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
  lifecycle: "stopped",
  endpointId: null,
  relay: null,
  admission: "none",
  geoConsent: "undecided",
  lastError: null,
  updatedMs: Date.now(),
};

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
      if (status.admission !== "granted" || status.lifecycle !== lifecycle) {
        setStatus({ admission: "granted", lifecycle, lastError: null });
      }
    } catch (e) {
      setStatus({ admission: "denied", lifecycle: "degraded", lastError: String((e && e.message) || e) });
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
    setStatus({ admission: "granted", lastError: null });
    scheduleRenew(addrJson, endpointId);
  } catch (e) {
    setStatus({ lifecycle: "error", admission: "denied", lastError: String((e && e.message) || e) });
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
  setStatus({ lifecycle: "stopped", admission: "none", endpointId: null, relay: null, lastError: null });
}

self.onconnect = (event) => {
  const port = event.ports[0];
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
  port.start();
  try {
    port.postMessage({ type: "status", status });
  } catch {
    /* ignore */
  }
};
