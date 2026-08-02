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
const WASM_BUNDLE_VERSION = 2; // v2: BrowserNode.boot()'s relay param is now a comma-separated list

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

// Epoch fencing (bn-p2p-reconnect-state, bn-p2p-resurrection-after-stop):
// start()'s async boot (wasm init + BrowserNode.boot, seconds on a cold
// fetch), the scheduled renewal tick, and the network-restore admit all
// re-check `closing`/`node` only BEFORE their await, never after — so a
// stop() that lands while any of them is in flight is invisible to the
// continuation that resumes afterward, and it can resurrect a node the user
// just explicitly cancelled (repro: click Start, click Stop within the init
// window). `closing` alone can't fence this: it is cleared back to `false`
// at the end of stop() itself, so a start() continuation checking it after
// stop() has already finished sees `closing === false` again and proceeds
// anyway. `epoch` is bumped by EVERY start()/stop() call and captured by
// value before each async gap; any continuation whose captured epoch no
// longer matches the live counter is stale and must undo whatever it just
// created (close a booted node, revoke an admission) instead of publishing
// it.
let epoch = 0;

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
  // A crashed/force-closed tab never sends `unloading` (that's a normal
  // pagehide/beforeunload path, which a crash skips entirely), so a
  // postMessage throw here is the ONLY signal such a port is gone. Previously
  // swallowed with a comment claiming lazy pruning existed elsewhere — it
  // didn't, so a dead port sat in `ports` forever, `ports.length===0` never
  // became true, and the last-tab auto-stop in onPortVisibility could never
  // fire. Collected during the loop (never splice while iterating) and
  // pruned the same way an explicit `unloading` message does.
  const dead = [];
  for (const p of ports) {
    try {
      p.postMessage({ type: "status", status });
    } catch {
      dead.push(p);
    }
  }
  if (dead.length) {
    for (const p of dead) {
      portVisibility.delete(p);
      const idx = ports.indexOf(p);
      if (idx !== -1) ports.splice(idx, 1);
    }
    if (ports.length === 0 && (node || status.lifecycle === "starting")) {
      stop();
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

// Single-in-flight-dial fencing (bn-p2p-reconnect-state): the scheduled
// renewal tick and a network-restore event are two INDEPENDENT triggers that
// can both decide to call admitOnce around the same moment (a network drop
// recovering right as the 60s timer also fires). The backend admission
// endpoint is itself idempotent (a fresh grant simply replaces the old one),
// so two concurrent requests were never a CORRECTNESS bug, but they're a real
// wasted-request race with no reason to exist — this makes it structurally
// impossible instead of merely harmless.
let renewInFlight = false;

// Decorrelated-jitter retry backoff (bn-p2p-reconnect-state, cross-checked
// against reconnecting-websocket/socket.io/node-retry/iroh's own relay
// actor.rs): sleepN = min(CAP, uniform(BASE, sleepN-1*3)). AWS's own
// comparison of jitter strategies ranks decorrelated the fastest-completing
// of the jittered options and unjittered backoff the outright loser — a
// FLAT retry interval on every failure (the previous behavior) is exactly
// the unjittered case. CAP is deliberately smaller than the general
// reconnect ladder would use: it exists so several retries still fit inside
// the lease window instead of burning most of it waiting out one full
// RENEW_INTERVAL_MS after a single transient failure.
const RENEW_BACKOFF_BASE_MS = 250;
const RENEW_BACKOFF_CAP_MS = 5_000;

function nextDecorrelatedJitterMs(prevMs) {
  const hi = Math.max(RENEW_BACKOFF_BASE_MS, prevMs * 3);
  return Math.min(RENEW_BACKOFF_CAP_MS, RENEW_BACKOFF_BASE_MS + Math.random() * (hi - RENEW_BACKOFF_BASE_MS));
}

function scheduleRenew(addrJson, endpointId) {
  if (renewTimer) clearTimeout(renewTimer);
  const myEpoch = epoch;
  let backoffMs = RENEW_BACKOFF_BASE_MS;

  async function tick() {
    if (closing || !node || myEpoch !== epoch) return; // session over — stop rescheduling
    if (renewInFlight) {
      // onNetworkChange's out-of-band admit is currently in flight — don't
      // duplicate it, but this self-rescheduling chain must keep going or it
      // dies silently (unlike the old setInterval, nothing else re-fires
      // this tick).
      renewTimer = setTimeout(tick, RENEW_BACKOFF_BASE_MS);
      return;
    }
    renewInFlight = true;
    const renewalTeam = session && session.team;
    try {
      await admitOnce(addrJson, endpointId);
      if (myEpoch !== epoch) {
        // stop() ran while this renewal's POST was in flight — it already
        // revoked whatever admission existed BEFORE this call, but this call
        // itself just re-created/refreshed one afterward. Undo it, same as
        // start()'s own stale-epoch handling, or the user sees "stopped"
        // while a live grant for this endpoint quietly lives out its lease.
        await discardStaleAttempt(null, endpointId, renewalTeam);
        return;
      }
      backoffMs = RENEW_BACKOFF_BASE_MS; // ladder resets on any success
      // Derive lifecycle from CURRENT visibility rather than hardcoding
      // "online" — a background renewal tick must not un-suspend a node
      // whose tabs are all still hidden.
      const lifecycle = anyVisible() ? "online" : "suspended";
      if (status.admission !== "granted" || status.lifecycle !== lifecycle || status.protocolMismatch !== "none") {
        setStatus({ admission: "granted", lifecycle, protocolMismatch: "none", lastError: null });
      }
      renewTimer = setTimeout(tick, RENEW_INTERVAL_MS);
    } catch (e) {
      if (myEpoch !== epoch) return;
      const message = String((e && e.message) || e);
      const mismatch = classifyAdmissionError(message);
      if (mismatch === "outdated") {
        // No retry will ever succeed against this server's floor — stop
        // spending renewal cycles on a doomed request and let the UI prompt
        // a reload instead of silently failing every 60s forever.
        setStatus({ admission: "denied", lifecycle: "error", protocolMismatch: mismatch, lastError: message });
        return; // terminal — no reschedule
      }
      // "server_upgrading" (transient) and any other failure keep retrying,
      // now on the jittered ladder instead of waiting a full
      // RENEW_INTERVAL_MS — the next attempt may hit an already-upgraded
      // node or a transient failure may simply clear.
      setStatus({ admission: "denied", lifecycle: "degraded", protocolMismatch: mismatch, lastError: message });
      backoffMs = nextDecorrelatedJitterMs(backoffMs);
      renewTimer = setTimeout(tick, backoffMs);
    } finally {
      renewInFlight = false;
    }
  }

  renewTimer = setTimeout(tick, RENEW_INTERVAL_MS);
}

// Discard a boot this epoch no longer owns: close/free a real node if one was
// created, best-effort revoke an admission if one was already granted. Never
// touches `node`/`status` — a newer epoch may already own those.
async function discardStaleAttempt(bootedNode, admittedEndpointId, team) {
  if (admittedEndpointId && team) {
    try {
      await callApi("DELETE", `/v1/browser/admissions/${encodeURIComponent(admittedEndpointId)}`, team);
    } catch {
      /* best-effort — the lease also expires on its own */
    }
  }
  if (bootedNode) {
    try {
      await bootedNode.close();
    } catch {
      /* already gone */
    }
    try {
      bootedNode.free();
    } catch {
      /* already gone */
    }
  }
}

async function start(msg) {
  if (node || status.lifecycle === "starting") return;
  const myEpoch = ++epoch;
  session = {
    deployment: msg.deployment,
    fn: msg.fn,
    digest: msg.digest,
    scope: msg.scope,
    team: msg.team,
    relay: msg.relay,
  };
  setStatus({ lifecycle: "starting", lastError: null, admission: "pending" });
  let booted = null;
  let admittedEndpointId = null;
  try {
    await init();
    if (myEpoch !== epoch) return; // stop() ran during wasm init
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
    if (myEpoch !== epoch) {
      await discardStaleAttempt(scratch, null, msg.team);
      return;
    }
    const { seedHex, created } = await loadOrCreateSeed(scratch.secretHex());
    let n;
    if (created) {
      n = scratch;
    } else {
      await scratch.close();
      scratch.free();
      n = await BrowserNode.boot(msg.relay, msg.discovery || null, seedHex);
    }
    booted = n;
    if (myEpoch !== epoch) {
      await discardStaleAttempt(booted, null, msg.team);
      return;
    }
    const endpointId = n.nodeId();
    const addrJson = n.addrJson();
    // Publish the node + endpointId ONLY once this epoch is confirmed still
    // current — everything above this line touched only locals, so a stale
    // epoch up to here has nothing to unpublish.
    node = n;
    setStatus({ endpointId, relay: msg.relay, lifecycle: anyVisible() ? "online" : "suspended" });
    await admitOnce(addrJson, endpointId);
    admittedEndpointId = endpointId;
    if (myEpoch !== epoch) {
      // stop() ran after we published `node` but before we could confirm the
      // admission — undo both: revoke the grant we just created and close
      // the node stop() itself never got to see, then leave `node`/`status`
      // alone (stop() already reset them to stopped/none).
      node = null;
      await discardStaleAttempt(booted, admittedEndpointId, msg.team);
      return;
    }
    setStatus({ admission: "granted", protocolMismatch: "none", lastError: null });
    scheduleRenew(addrJson, endpointId);
  } catch (e) {
    if (myEpoch !== epoch) {
      // A stale attempt failing is not this epoch's error to report.
      await discardStaleAttempt(booted, admittedEndpointId, msg.team);
      return;
    }
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

// Reconnect machine (bn-p2p-reconnect-state, one real slice of it): a tab
// reports real `online`/`offline` window events here (a Worker/SharedWorker
// has no `navigator.onLine` visibility of its own — it only knows what a
// connected tab tells it, same reasoning as the existing visibility
// reporting above). Starts true (optimistic) since the worker may boot
// before any tab's first report arrives.
let networkOnline = true;
function onNetworkChange(online) {
  const wasOnline = networkOnline;
  networkOnline = online;
  if (wasOnline === online) return;
  if (!online) {
    // A dropped network can't promise service regardless of what the
    // scheduled renewal timer still believes — degrade immediately rather
    // than waiting for the next tick's request to time out first.
    if (status.lifecycle === "online" || status.lifecycle === "suspended") {
      setStatus({ lifecycle: "degraded" });
    }
    return;
  }
  // Network restored: don't wait up to RENEW_INTERVAL_MS for the next
  // scheduled tick — retry the admission right away so a real reconnect is
  // fast, not just eventually-correct. Reuses scheduleRenew's own retry
  // machinery for the SUBSEQUENT tick rather than duplicating it; this is
  // only the one immediate out-of-band attempt. Guarded by the SAME
  // renewInFlight fence the scheduled tick uses, so this can never race a
  // tick that's already mid-flight — it just skips (the next scheduled tick,
  // or the next network event, covers it; nothing is lost, only deduped).
  const endpointId = status.endpointId;
  if (node && session && !closing && endpointId && !renewInFlight) {
    renewInFlight = true;
    const myEpoch = epoch;
    const networkTeam = session.team;
    admitOnce(node.addrJson(), endpointId)
      .then(() => {
        if (myEpoch !== epoch) {
          // stop() ran while this POST was in flight and already revoked
          // whatever admission existed before it — this call just
          // re-created one afterward. Undo it, same reasoning as
          // scheduleRenew's identical stale-success case.
          return discardStaleAttempt(null, endpointId, networkTeam);
        }
        const lifecycle = anyVisible() ? "online" : "suspended";
        setStatus({ admission: "granted", lifecycle, protocolMismatch: "none", lastError: null });
      })
      .catch((e) => {
        if (myEpoch !== epoch) return;
        const message = String((e && e.message) || e);
        setStatus({
          admission: "denied",
          lifecycle: "degraded",
          protocolMismatch: classifyAdmissionError(message),
          lastError: message,
        });
      })
      .finally(() => {
        renewInFlight = false;
      });
  }
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
  epoch++; // fences every in-flight start()/renew continuation from this point on
  closing = true;
  if (renewTimer) clearTimeout(renewTimer);
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
    else if (msg.type === "network") onNetworkChange(!!msg.online);
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
