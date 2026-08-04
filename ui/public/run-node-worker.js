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

import { loadOrCreateSeed } from "./browser-node/identity.js";
import { WorkerFunctionRuntime } from "./browser-node/worker-function-runtime.js";
import { DIGEST_RE } from "./browser-node/artifact-policy.js";

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
const WASM_BUNDLE_VERSION = 13; // v13: EOF-only stream completion, semaphore backpressure
const WASM_MODULE_URL = new URL(
  `./browser-node/pkg/hive_browser.js?v=${WASM_BUNDLE_VERSION}`,
  import.meta.url,
);
const WASM_BINARY_URL = new URL(
  `./browser-node/pkg/hive_browser_bg.wasm?v=${WASM_BUNDLE_VERSION}`,
  import.meta.url,
);
let browserModulePromise = null;

function loadBrowserModule() {
  if (browserModulePromise) return browserModulePromise;
  const pending = import(WASM_MODULE_URL.href).then(async (module) => {
    await module.default(WASM_BINARY_URL);
    return module;
  });
  browserModulePromise = pending;
  pending.catch(() => {
    if (browserModulePromise === pending) browserModulePromise = null;
  });
  return pending;
}

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

// browser-worker-quickjs-runtime: the worker-native QuickJS function lane.
// fnRuntime owns verified/pinned artifacts, per-artifact queues and the
// global caps; fnGrants mirrors exactly what has been granted on `node` via
// grantInvoker (policyDigest -> Set<callerEndpointId>) so every renewal can
// reconcile by set difference — revoking stale callers/digests BEFORE
// granting their replacements. Both are torn down together with `node`:
// stop(), a terminal admission failure, or an endpoint rotation; a worker
// crash needs no local teardown (the wasm grant map dies with the worker —
// the server-side lease expiry removes the route).
let fnRuntime = null;
let fnGrants = new Map();

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
  // Seeded from wall-clock time, not 0 (bn-ui-sharedworker-owner): a fresh
  // worker booted by Web Locks owner handoff has no way to know what
  // version number surviving tabs' OWN applyStatus (generation-fenced,
  // run-node-status.ts) is currently holding — starting at 0 meant every
  // subsequent status update from the new owner was silently dropped by
  // every surviving tab until this worker's own small integer counter
  // eventually climbed back past their old high-water mark (a real,
  // previously-measured "prolonged but self-healing" stall, not a
  // permanent one, but still a real multi-status-update gap). Date.now()
  // only moves forward, so a fresh worker's version is virtually always
  // already ahead of a same-origin worker's prior counter (a normal
  // integer increment would need decades to reach a Date.now()-scale
  // value), fixing the stall to be effectively instant instead of merely
  // eventual.
  version: Date.now(),
  abiVersion: HOST_ABI_VERSION,
  lifecycle: "stopped",
  endpointId: null,
  // bn-safari-sharedworker-cryptokey-dataclone: how this boot's identity is
  // held — "persistent" (wrapping key in IndexedDB, survives any restart) or
  // "memory" (wrapping key dies with this worker; identity survives this
  // worker's lifetime, NOT its restart on engines where a SharedWorker cannot
  // structured-clone a CryptoKey — Safari 26, witnessed). Surfaced honestly,
  // never hidden. Additive on the wire; the ui/lib/run-node-status.ts typed
  // mirror picks it up separately (that file is owned elsewhere).
  identityPersistence: null,
  relay: null,
  admission: "none",
  geoConsent: "undecided",
  protocolMismatch: "none",
  lastError: null,
  updatedMs: Date.now(),
  // Multi-tab dedup UI (bn-ui-sharedworker-owner): how many distinct tabs are
  // currently attached, real vs. assumed-just-this-one. See currentTabCount().
  tabCount: 1,
  // Auth-renewal input (bn-p2p-reconnect-state): true while the last renewal
  // failed specifically because this tab's PLATFORM session (not the node's
  // own identity/protocol) is stale — see isSessionStaleError. Self-clears on
  // the next successful renewal; never blocks retry (see that function's doc).
  sessionStale: false,
  // browser-worker-quickjs-runtime introspection (additive): the pinned
  // artifact digests, live invoker-grant count and served-invoke total of the
  // worker's QuickJS lane. null while no function runtime is running.
  functions: null,
};

// bn-p2p-version-negotiation: the backend prefixes its two protocol-mismatch
// rejections with a stable marker (see crates/hive-cloud/src/browser_admission.rs
// validate_request) so the two directions get distinct client treatment
// instead of both looking like a generic admission failure.
function classifyAdmissionError(error) {
  const code = error && typeof error === "object" ? error.code : null;
  const message = String((error && error.message) || error || "");
  if (code === "protocol_too_old" || message.startsWith("protocol_too_old")) return "outdated";
  if (code === "protocol_too_new" || message.startsWith("protocol_too_new")) return "server_upgrading";
  // Same remedy as "outdated" (a reload re-fetches the wasm bundle fresh) --
  // distinct root cause (a stale LOCAL cache, not the fleet's protocol
  // floor), same UI treatment either way.
  if (message.startsWith("wasm_bundle_stale")) return "outdated";
  return "none";
}

// bn-p2p-reconnect-state (auth-renewal input): the backend's fresh_user_claims
// rejects a renewal once this tab's PLATFORM session (not the browser-node's
// own identity or protocol version) has aged past HIVE_BROWSER_SESSION_MAX_AGE_SECS
// or genuinely expired. Distinct from classifyAdmissionError's protocol-version
// concerns on purpose — this is auth staleness, unrelated to wire compatibility.
// Deliberately NOT terminal like "outdated": callApi uses `credentials:
// "same-origin"`, so if the platform's own session-cookie refresh (e.g. Clerk's
// background token rotation) updates the cookie before this tab's next retry,
// the very next renewal attempt succeeds with no reload needed — treating this
// as a permanent failure would stop retrying right before a self-heal that was
// about to happen. Exists so the UI can show a calm "reconnecting" state
// instead of the same alarming red error every other unclassified failure gets.
function isSessionStaleError(error) {
  const code = error && typeof error === "object" ? error.code : null;
  const message = String((error && error.message) || error || "");
  if (code) {
    return code === "session_required" || code === "session_stale" || code === "session_lease_too_short";
  }
  return message.includes("session is expired or not fresh") || message.includes("requires a fresh interactive user session");
}

function isTerminalAdmissionError(error) {
  if (error && typeof error === "object" && typeof error.retryable === "boolean") {
    return !error.retryable;
  }
  // Backward-compatible during a rolling deploy: old nodes return plain text.
  // Only the already-stable too-old marker is terminal without structured
  // metadata; every other old response keeps retrying rather than guessing.
  return classifyAdmissionError(error) === "outdated";
}

function broadcast() {
  status = { ...status, version: status.version + 1, updatedMs: Date.now(), tabCount: currentTabCount() };
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

// DIAL_DEADLINE from the reconnect-machine design (bn-p2p-reconnect-state):
// callApi previously had no client-side timeout at all, relying entirely on
// the browser's own (much longer, and not something this code controls) TCP/
// fetch timeout -- a hung admission request could block the reconnect chain
// far longer than any of the backoff/jitter timing around it was designed
// for. AbortController is the standard fetch-timeout primitive; a timeout
// abort surfaces as a real thrown error (name "AbortError"), which the
// existing catch/classifyAdmissionError paths already handle like any other
// admitOnce failure -- no special-casing needed there.
const DIAL_DEADLINE_MS = 10_000;

async function callApi(method, path, team, body) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), DIAL_DEADLINE_MS);
  try {
    const r = await fetch(`/cloud${path}`, {
      method,
      credentials: "same-origin",
      headers: { "content-type": "application/json", "x-hive-team": team },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: controller.signal,
    });
    if (!r.ok) {
      const contentType = String(r.headers.get("content-type") || "").toLowerCase();
      const text = await r.text().catch(() => "");
      let detail = null;
      // The structured error field is content-type-gated: only a body the
      // server actually LABELLED as JSON is parsed as one. A proxy/static
      // origin's error page (e.g. a bare 501) is text/html.
      if (contentType.includes("json") && text) {
        try {
          const payload = JSON.parse(text);
          detail = payload && payload.error && typeof payload.error === "object" ? payload.error : null;
        } catch {
          // Malformed JSON error body — fall through to the bounded reason.
        }
      }
      // bn-worker-error-body-html-leak: a raw error body is NEVER copied into
      // the message verbatim — this string lands in the dashboard status via
      // lastError, and an HTML error page would render there tags and all.
      // The only body-derived reason allowed is short, single-line and
      // markup-free (rollout-compat: pre-structured-error nodes return plain
      // text); everything else collapses to the bare status line.
      const plain =
        !detail && text && !contentType.includes("html") && text.length <= 200 && !/[<>\r\n]/.test(text)
          ? text.trim()
          : null;
      const reason =
        (detail && detail.message) ||
        plain ||
        `${method} ${path} -> ${r.status}${r.statusText ? ` ${r.statusText}` : ""}`;
      const error = new Error(String(reason).slice(0, 300));
      error.code = detail && typeof detail.code === "string" ? detail.code : null;
      error.retryable = detail && typeof detail.retryable === "boolean" ? detail.retryable : null;
      error.status = r.status;
      throw error;
    }
    return await r.json();
  } finally {
    clearTimeout(timer);
  }
}

async function admitOnce(addrJson, endpointId) {
  const { deployment, fn, digest, scope, team } = session;
  // Proof-of-possession (bn-p2p-heartbeat-lease): prove THIS call controls
  // endpointId's private key, not just that the caller has a valid platform
  // session -- without this an admission naming any endpoint_id was accepted
  // on platform auth alone. challengeMs is this tab's own clock, no separate
  // nonce round trip; the backend bounds replay to a tight freshness window
  // around it (see verify_proof_of_possession's HIVE_BROWSER_POP_WINDOW_MS).
  const challengeMs = Date.now();
  const signature = node.signAdmission(String(challengeMs));
  // The response carries the server-derived capability block (artifact
  // descriptor + trusted caller set); the caller reconciles from it.
  return await callApi("POST", "/v1/browser/admissions", team, {
    endpoint_id: endpointId,
    addr_json: addrJson,
    deployment,
    function: fn,
    digest,
    scope: scope || "team",
    protocol_version: PROTOCOL_VERSION,
    challenge_ms: challengeMs,
    signature,
  });
}

// ---------------------------------------------------------------------------
// browser-worker-quickjs-runtime: capability reconciliation + artifact pin.
//
// The admission response's capability block is the ONLY source of what this
// worker may serve: artifact_url/policy_digest/source_digest/source_bytes/
// mode/limits/allowed_ops describe the artifact; trusted_callers is the exact
// fleet EndpointId set to grant. The worker never accepts a digest, a byte,
// or a caller id from anywhere else.
// ---------------------------------------------------------------------------

const ARTIFACT_FETCH_DEADLINE_MS = 15_000;
const ARTIFACT_CACHE_NAME = "hive-browser-artifacts-v1";

function artifactRequestUrl(policyDigest) {
  return `/cloud/v1/browser/artifacts/${policyDigest}`;
}

// Validate the capability block's shape into a local descriptor. Everything
// not matching is a hard error — a malformed capability pins nothing.
function normalizeCapability(capability) {
  if (!capability || typeof capability !== "object") {
    throw new Error("admission response is missing its capability block");
  }
  const descriptor = {
    policyDigest: capability.policy_digest,
    sourceDigest: capability.source_digest,
    sourceBytes: capability.source_bytes,
    mode: capability.mode,
    timeoutMs: capability.timeout_ms,
    memoryBytes: capability.memory_bytes,
    stackBytes: capability.stack_bytes,
    allowedOps: capability.allowed_ops,
  };
  if (!DIGEST_RE.test(descriptor.policyDigest || "") || !DIGEST_RE.test(descriptor.sourceDigest || "")) {
    throw new Error("capability carries a malformed digest");
  }
  if (descriptor.mode !== "quickjs") {
    throw new Error(`capability mode ${JSON.stringify(descriptor.mode)} is unsupported — the worker lane serves quickjs artifacts only`);
  }
  if (!Number.isSafeInteger(descriptor.sourceBytes) || descriptor.sourceBytes <= 0) {
    throw new Error("capability source_bytes must be a positive integer");
  }
  if (!Array.isArray(descriptor.allowedOps)) throw new Error("capability allowed_ops must be an array");
  // artifact_url is server-derived but still checked against the ONLY shape
  // the delivery contract defines before it is ever fetched.
  const expectedUrl = `/v1/browser/artifacts/${descriptor.policyDigest}`;
  if (capability.artifact_url !== expectedUrl) {
    throw new Error("capability artifact_url does not match its policy digest");
  }
  // trusted_callers is server-derived (healthy fleet EndpointIds); filter to
  // the exact 64-hex shape anyway — grantInvoker accepts nothing else, and a
  // malformed entry must never reach the grant map.
  const trustedCallers = Array.isArray(capability.trusted_callers) ? capability.trusted_callers : [];
  const callers = [...new Set(trustedCallers.filter(id => typeof id === "string" && DIGEST_RE.test(id)))];
  return { descriptor, callers };
}

// Fetch the artifact body with a bounded deadline through the same
// authenticated /cloud proxy path as callApi. Returns raw bytes — verified by
// pin() before anything executes or persists.
async function fetchArtifactBytes(descriptor, team) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), ARTIFACT_FETCH_DEADLINE_MS);
  try {
    const r = await fetch(artifactRequestUrl(descriptor.policyDigest), {
      credentials: "same-origin",
      headers: { "x-hive-team": team },
      signal: controller.signal,
    });
    if (!r.ok) throw new Error(`artifact fetch failed: ${r.status}`);
    return new Uint8Array(await r.arrayBuffer());
  } finally {
    clearTimeout(timer);
  }
}

// Cache Storage holds ONLY verified bytes: entries are written solely after a
// successful pin (which recomputes both digests), and a cache read is never
// trusted — the bytes go through the same pin() verification as network
// bytes. A cache that fails verification is deleted and refetched.
async function readCachedArtifact(descriptor) {
  if (typeof caches === "undefined") return null;
  try {
    const cache = await caches.open(ARTIFACT_CACHE_NAME);
    const hit = await cache.match(artifactRequestUrl(descriptor.policyDigest));
    if (!hit) return null;
    return new Uint8Array(await hit.arrayBuffer());
  } catch {
    return null;
  }
}

async function persistVerifiedArtifact(descriptor, bytes) {
  if (typeof caches === "undefined") return;
  try {
    const cache = await caches.open(ARTIFACT_CACHE_NAME);
    await cache.put(
      artifactRequestUrl(descriptor.policyDigest),
      new Response(bytes, { headers: { "content-length": String(bytes.length) } }),
    );
  } catch {
    /* quota or storage failure — the next renewal simply refetches */
  }
}

async function dropCachedArtifact(policyDigest) {
  if (typeof caches === "undefined") return;
  try {
    const cache = await caches.open(ARTIFACT_CACHE_NAME);
    await cache.delete(artifactRequestUrl(policyDigest));
  } catch {
    /* best effort */
  }
}

// One atomic reconcile per admission/renewal response. Verification fully
// precedes mutation: the artifact is fetched + pinned (both digests
// recomputed locally) BEFORE any grant changes, then stale callers/digests
// are revoked BEFORE their replacements are granted — a failed reconcile
// leaves the previous pin + grants exactly as they were.
async function reconcileCapability(capability, myEpoch) {
  const runtime = fnRuntime;
  const owner = node;
  if (!runtime || runtime.closed || !owner) throw new Error("function runtime is not running");
  const team = session && session.team;
  const { descriptor, callers } = normalizeCapability(capability);

  if (!runtime.has(descriptor.policyDigest)) {
    let bytes = await readCachedArtifact(descriptor);
    if (bytes) {
      try {
        runtime.pin(descriptor, bytes);
      } catch {
        await dropCachedArtifact(descriptor.policyDigest);
        bytes = null;
      }
    }
    if (!bytes) {
      bytes = await fetchArtifactBytes(descriptor, team);
      if (myEpoch !== epoch || node !== owner) return; // a newer epoch owns teardown now
      runtime.pin(descriptor, bytes); // throws on any digest/size mismatch
      await persistVerifiedArtifact(descriptor, bytes);
    }
  }
  if (myEpoch !== epoch || node !== owner) return;

  // Revoke stale digests (close their runners) and stale callers FIRST.
  for (const [digest, granted] of [...fnGrants]) {
    if (digest === descriptor.policyDigest) continue;
    for (const caller of granted) {
      try {
        owner.revokeInvoker(caller, digest);
      } catch {
        /* node already closed — the grant map died with it */
      }
    }
    fnGrants.delete(digest);
    runtime.unpin(digest);
  }
  const desired = new Set(callers);
  const current = fnGrants.get(descriptor.policyDigest) || new Set();
  for (const caller of [...current]) {
    if (desired.has(caller)) continue;
    try {
      owner.revokeInvoker(caller, descriptor.policyDigest);
    } catch {
      /* node already closed */
    }
    current.delete(caller);
  }
  // Only now grant replacements: every exact returned caller EndpointId, for
  // only the returned policy digest.
  for (const caller of desired) {
    if (current.has(caller)) continue;
    owner.grantInvoker(caller, descriptor.policyDigest);
    current.add(caller);
  }
  fnGrants.set(descriptor.policyDigest, current);
  updateFunctionsStatus();
}

function updateFunctionsStatus() {
  if (!fnRuntime || fnRuntime.closed) {
    if (status.functions !== null) setStatus({ functions: null });
    return;
  }
  const stats = fnRuntime.stats();
  let grants = 0;
  for (const callers of fnGrants.values()) grants += callers.size;
  setStatus({ functions: { pinned: stats.pinned, grants, served: stats.servedTotal } });
}

// Every teardown path (stop, terminal admission failure, endpoint rotation)
// funnels here: close runners, revoke every outstanding grant, forget pins.
// `owner` is the node the grants were applied to (the terminal path passes
// its doomed node explicitly after clearing `node`).
function teardownFunctionLane(owner = node) {
  if (owner) {
    for (const [digest, callers] of fnGrants) {
      for (const caller of callers) {
        try {
          owner.revokeInvoker(caller, digest);
        } catch {
          /* node already closed — its wasm grant map is gone anyway */
        }
      }
    }
  }
  fnGrants = new Map();
  if (fnRuntime) {
    try {
      fnRuntime.close();
    } catch {
      /* already closed */
    }
    fnRuntime = null;
  }
}

// One renewal spine owns every admission write: periodic lease refresh,
// browser-network restoration, and iroh home-relay/address migration. The
// attempt reads currentAddrJson at execution time; boot's first address is
// never captured forever.
let renewInFlightEpoch = null;
let renewKickPending = false;
let currentAddrJson = null;
let admittedAddrJson = null;

// Decorrelated-jitter retry backoff (bn-p2p-reconnect-state, cross-checked
// against reconnecting-websocket/socket.io/node-retry/iroh's own relay
// actor.rs): sleepN = min(CAP, uniform(BASE, sleepN-1*3)).
const RENEW_BACKOFF_BASE_MS = 250;
const RENEW_BACKOFF_CAP_MS = 5_000;
let renewBackoffMs = RENEW_BACKOFF_BASE_MS;

function nextDecorrelatedJitterMs(prevMs) {
  const hi = Math.max(RENEW_BACKOFF_BASE_MS, prevMs * 3);
  return Math.min(RENEW_BACKOFF_CAP_MS, RENEW_BACKOFF_BASE_MS + Math.random() * (hi - RENEW_BACKOFF_BASE_MS));
}

function scheduleRenew(delayMs) {
  if (renewTimer) clearTimeout(renewTimer);
  const myEpoch = epoch;
  renewTimer = setTimeout(() => renewNow(myEpoch), delayMs);
}

function kickRenew() {
  renewKickPending = true;
  if (renewInFlightEpoch === epoch) return;
  renewKickPending = false;
  scheduleRenew(0);
}

async function renewNow(myEpoch) {
  if (closing || !node || !session || myEpoch !== epoch) return "stale";
  if (!networkOnline) return "offline";
  if (renewInFlightEpoch === myEpoch) {
    renewKickPending = true;
    return "pending";
  }
  if (!currentAddrJson) {
    if (status.lifecycle !== "degraded" || status.relay !== null) {
      setStatus({ lifecycle: "degraded", relay: null, lastError: "No browser relay is connected — reconnecting automatically." });
    }
    renewBackoffMs = nextDecorrelatedJitterMs(renewBackoffMs);
    scheduleRenew(renewBackoffMs);
    return "offline";
  }

  const attemptedAddr = currentAddrJson;
  const endpointId = status.endpointId;
  const renewalTeam = session.team;
  renewInFlightEpoch = myEpoch;
  renewKickPending = false;
  let nextDelay = null;
  let outcome = "retrying";
  try {
    const admitResult = await admitOnce(attemptedAddr, endpointId);
    if (myEpoch !== epoch) {
      await discardStaleAttempt(null, endpointId, renewalTeam);
      return "stale";
    }
    // Capability reconcile rides the same renewal spine: pin/re-verify the
    // server-described artifact, then revoke stale callers/digests before
    // granting their replacements. A reconcile failure is treated exactly
    // like an admission failure (backoff retry) — the previous pin + grants
    // are left untouched.
    await reconcileCapability(admitResult && admitResult.capability, myEpoch);
    if (myEpoch !== epoch) return "stale";
    admittedAddrJson = attemptedAddr;
    renewBackoffMs = RENEW_BACKOFF_BASE_MS;
    const lifecycle = anyVisible() ? "online" : "suspended";
    setStatus({ admission: "granted", lifecycle, protocolMismatch: "none", sessionStale: false, lastError: null });
    nextDelay = RENEW_INTERVAL_MS;
    outcome = "success";
  } catch (error) {
    if (myEpoch !== epoch) return "stale";
    const message = String((error && error.message) || error);
    const mismatch = classifyAdmissionError(error);
    const sessionStale = isSessionStaleError(error);
    const terminal = isTerminalAdmissionError(error);
    setStatus({
      admission: "denied",
      lifecycle: terminal ? "error" : "degraded",
      protocolMismatch: mismatch,
      sessionStale,
      lastError: message,
    });
    if (terminal) {
      const doomed = node;
      node = null;
      currentAddrJson = null;
      admittedAddrJson = null;
      teardownFunctionLane(doomed);
      await discardStaleAttempt(doomed, endpointId, renewalTeam);
      outcome = "terminal";
    } else {
      renewBackoffMs = nextDecorrelatedJitterMs(renewBackoffMs);
      nextDelay = renewBackoffMs;
    }
  } finally {
    if (renewInFlightEpoch === myEpoch) renewInFlightEpoch = null;
    if (myEpoch === epoch && node) {
      if (currentAddrJson !== attemptedAddr) renewKickPending = true;
      if (renewKickPending) {
        renewKickPending = false;
        scheduleRenew(RENEW_BACKOFF_BASE_MS);
      } else if (nextDelay !== null) {
        scheduleRenew(nextDelay);
      }
    }
  }
  return outcome;
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

// Health/latency-aware relay ordering (bn-p2p-relay-failover): iroh's own
// RelayMap can carry several relays, but nothing upstream of it knows which
// one is actually CLOSEST to this browser -- a fixed config order (as
// deployed: sanjose, bangkok, virginia) is not a proximity order (measured
// live from a US-west-coast vantage: sanjose ~120ms, virginia ~360ms,
// bangkok ~880ms -- bangkok, listed SECOND, is actually the slowest of the
// three). Probes each relay's real HTTPS port with a short timeout and
// returns the SAME urls sorted fastest-first; a relay that fails/times out
// is dropped from the front of the list but not lost entirely (appended
// after the successful ones, in original order) -- still worth trying if
// every faster one is also down, never silently removed. Falls back to the
// UNCHANGED original string if every probe fails (offline, or every relay
// genuinely down) -- boot() gets the same list either way, just possibly
// unordered, which is exactly today's pre-existing behavior, never worse.
const RELAY_PROBE_TIMEOUT_MS = 3_000;

async function orderRelaysByLatency(relayUrlsCsv) {
  const urls = relayUrlsCsv
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  if (urls.length <= 1) return relayUrlsCsv;
  const timed = await Promise.all(
    urls.map(async (url) => {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), RELAY_PROBE_TIMEOUT_MS);
      const started = performance.now();
      try {
        // mode:"no-cors" is load-bearing: this is a cross-origin fetch (the
        // relay is not a CORS-enabled API, it never sends Access-Control-
        // Allow-Origin) and the DEFAULT "cors" mode throws on a response
        // with no CORS headers -- discovered live, every probe silently
        // failed and this function always fell back to the unordered
        // original list until this fix. no-cors still requires the server
        // to actually respond before the promise settles, so elapsed time
        // is a real latency measurement even though the (opaque) response
        // body/status can't be read.
        await fetch(url, { method: "GET", mode: "no-cors", signal: controller.signal });
        return { url, ms: performance.now() - started };
      } catch {
        return { url, ms: null };
      } finally {
        clearTimeout(timer);
      }
    }),
  );
  const reachable = timed.filter((t) => t.ms !== null).sort((a, b) => a.ms - b.ms);
  const unreachable = timed.filter((t) => t.ms === null);
  if (reachable.length === 0) return relayUrlsCsv; // every probe failed — leave order unchanged
  return [...reachable, ...unreachable].map((t) => t.url).join(",");
}

function onAddressChange(owner, myEpoch, json) {
  if (myEpoch !== epoch || node !== owner || closing) return;
  let update;
  try {
    update = JSON.parse(json);
  } catch {
    setStatus({ lifecycle: "degraded", relay: null, lastError: "Browser relay status was malformed — reconnecting automatically." });
    return;
  }
  const relays = Array.isArray(update.relays) ? update.relays.filter((relay) => typeof relay === "string") : [];
  const nextAddr = update.online === true && typeof update.addrJson === "string" && update.addrJson ? update.addrJson : null;
  const changed = nextAddr !== currentAddrJson;
  currentAddrJson = nextAddr;
  if (!nextAddr || relays.length === 0) {
    admittedAddrJson = null;
    setStatus({
      lifecycle: "degraded",
      relay: null,
      lastError: "No browser relay is connected — reconnecting automatically.",
    });
    return;
  }
  const relay = relays.join(",");
  if (changed || admittedAddrJson !== nextAddr) {
    setStatus({ lifecycle: "degraded", relay, lastError: null });
    kickRenew();
  } else if (status.relay !== relay) {
    setStatus({ relay });
  }
}

async function start(msg) {
  if (node || status.lifecycle === "starting") return;
  const myEpoch = ++epoch;
  currentAddrJson = null;
  admittedAddrJson = null;
  renewKickPending = false;
  renewBackoffMs = RENEW_BACKOFF_BASE_MS;
  session = {
    deployment: msg.deployment,
    fn: msg.fn,
    digest: msg.digest,
    scope: msg.scope,
    team: msg.team,
    relay: msg.relay,
  };
  setStatus({ lifecycle: "starting", lastError: null, admission: "pending", sessionStale: false });
  let booted = null;
  try {
    const orderedRelay = await orderRelaysByLatency(msg.relay);
    if (myEpoch !== epoch) return; // stop() ran during the relay probe
    const { BrowserNode, wasmBundleVersion, blake3Hex } = await loadBrowserModule();
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
    // `persistence` (bn-safari-sharedworker-cryptokey-dataclone) reports WHICH
    // custody mode holds the identity — "memory" means a worker restart on
    // this engine forfeits it; surfaced in status below, never hidden.
    const scratch = await BrowserNode.boot(orderedRelay, null, null);
    booted = scratch;
    if (myEpoch !== epoch) {
      await discardStaleAttempt(scratch, null, msg.team);
      return;
    }
    const { seedHex, created, persistence } = await loadOrCreateSeed(scratch.secretHex());
    let n;
    if (created) {
      n = scratch;
    } else {
      await scratch.close();
      scratch.free();
      booted = null;
      n = await BrowserNode.boot(orderedRelay, msg.discovery || null, seedHex);
    }
    booted = n;
    if (myEpoch !== epoch) {
      await discardStaleAttempt(booted, null, msg.team);
      return;
    }
    const endpointId = n.nodeId();
    // browser-worker-quickjs-runtime: create the worker-native QuickJS lane
    // and install the invoke handler BEFORE this endpoint can become routable
    // — the first admission only happens after setAddressHandler -> kickRenew
    // below. The handler resolves exclusively against locally pinned,
    // server-described, BLAKE3-verified artifacts; an unpinned digest
    // rejects, so installing early grants nobody anything.
    const runtime = new WorkerFunctionRuntime({
      blake3: blake3Hex,
      ops: {
        // The platform registry's read ops with worker-meaningful handlers
        // (hive-browser/identity-json-v1, hive-browser/utf8-array-buffer-v1);
        // per artifact the admission policy's allowed_ops narrows further.
        1: async (value) => value,
        2: async (value) => new TextEncoder().encode(String(value)).buffer,
      },
    });
    n.setInvokeHandler((digest, request) => {
      if (fnRuntime !== runtime || runtime.closed) {
        return Promise.reject(new Error("function runtime is not running"));
      }
      return runtime.invoke(digest, request);
    });
    fnRuntime = runtime;
    fnGrants = new Map();
    // Publish the node + endpointId ONLY once this epoch is confirmed still
    // current. setAddressHandler synchronously emits iroh's current address,
    // then keeps the same callback alive for every relay migration.
    node = n;
    setStatus({ endpointId, relay: null, lifecycle: "starting", identityPersistence: persistence || null });
    n.setAddressHandler((json) => onAddressChange(n, myEpoch, json));
  } catch (error) {
    if (myEpoch !== epoch) {
      await discardStaleAttempt(booted, null, msg.team);
      return;
    }
    const message = String((error && error.message) || error);
    node = null;
    currentAddrJson = null;
    admittedAddrJson = null;
    fnRuntime = null;
    fnGrants = new Map();
    if (booted) await discardStaleAttempt(booted, null, msg.team);
    setStatus({
      lifecycle: "error",
      admission: "denied",
      protocolMismatch: classifyAdmissionError(error),
      sessionStale: isSessionStaleError(error),
      lastError: message,
    });
  }
}

function anyVisible() {
  if (portVisibility.size === 0) return true; // no reports yet — assume foreground
  for (const visible of portVisibility.values()) if (visible) return true;
  return false;
}

// Multi-tab dedup UI (bn-ui-sharedworker-owner, the row's 3rd remaining item):
// a real per-tab count, not a guess. In SharedWorker mode `ports` already
// holds one real MessagePort per connected tab and portVisibility is keyed by
// those same port objects, so the two naturally agree. Under the Web Locks
// fallback only ONE real transport exists (`self`, owned by the winning tab)
// but every tab -- owner and observers alike -- now tags its own visibility
// reports with a stable per-tab id (see onPortVisibility below), so
// portVisibility.size is the true distinct-tab count there instead of always
// reading 1. Falls back to ports.length (never 0 — this worker itself implies
// at least one attached tab) before any report has arrived yet.
function currentTabCount() {
  return portVisibility.size > 0 ? portVisibility.size : Math.max(ports.length, 1);
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
    if (renewTimer) clearTimeout(renewTimer);
    renewTimer = null;
    if (status.lifecycle === "online" || status.lifecycle === "suspended") {
      setStatus({ lifecycle: "degraded", lastError: "Network unavailable — reconnecting automatically." });
    }
    return;
  }
  // The same renewal spine handles this as relay migration and periodic refresh;
  // an in-flight older trigger records a pending kick instead of dropping it.
  if (node && session && !closing && currentAddrJson) kickRenew();
}

function onPortVisibility(port, msg) {
  // Web Locks fallback fan-in (bn-ui-sharedworker-owner): several tabs share
  // ONE real transport (`self`, owned by the tab that won the lock) — the
  // owner's own visibility reports and every non-owner observer's reports
  // (relayed through the owner's controlChannel forward) all arrive here as
  // the SAME `port` argument. Tagging each report with the sending tab's own
  // stable id lets them be tracked independently in portVisibility instead of
  // colliding on one key and losing every-but-the-last tab's signal. This
  // path NEVER touches `ports` or calls stop() — losing one observer's
  // visibility is not the same as the owner's actual connection to the worker
  // going away (that untagged case, below, is unchanged from before).
  if (msg.tabId) {
    if (msg.unloading) portVisibility.delete(msg.tabId);
    else portVisibility.set(msg.tabId, !!msg.visible);
  } else if (msg.unloading) {
    portVisibility.delete(port);
    const idx = ports.indexOf(port);
    if (idx !== -1) ports.splice(idx, 1);
    // No tab left that can observe or control this node — release it rather
    // than run headless forever with no way for a user to ever stop it.
    if (ports.length === 0 && (node || status.lifecycle === "starting")) {
      stop();
      return;
    }
  } else {
    portVisibility.set(port, !!msg.visible);
  }
  // Only toggle between online/suspended — never override starting/error/
  // stopped, which are driven by the connect/admit lifecycle itself.
  if (status.lifecycle === "online" && !anyVisible()) {
    setStatus({ lifecycle: "suspended" });
  } else if (status.lifecycle === "suspended" && anyVisible()) {
    setStatus({ lifecycle: "online" });
  } else {
    // Neither transition applies (e.g. tabCount changed but visibility
    // didn't) — still worth a broadcast so tabCount stays live in the UI.
    broadcast();
  }
}

async function stop() {
  epoch++; // fences every in-flight start()/renew continuation from this point on
  closing = true;
  if (renewTimer) clearTimeout(renewTimer);
  renewTimer = null;
  renewKickPending = false;
  currentAddrJson = null;
  admittedAddrJson = null;
  const endpointId = status.endpointId;
  const team = session && session.team;
  if (node) {
    // Revoke every invoker grant + close every runner BEFORE the admission
    // DELETE and the node close (which itself clears the wasm grant map).
    teardownFunctionLane(node);
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
    sessionStale: false,
    endpointId: null,
    identityPersistence: null,
    relay: null,
    lastError: null,
    functions: null,
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
  // detection (an engine with no SharedWorker at all, or an explicit
  // capability failure) but won the navigator.locks single-owner election in
  // ui/lib/use-run-node.ts. NOT "Safari private mode": Safari 26 HAS
  // SharedWorker in normal AND private windows (witnessed live, Safari
  // 26.3.1) — modern Safari takes the SharedWorker branch, and its remaining
  // context quirk (a CryptoKey is not structured-cloneable inside a
  // SharedWorker) is handled inside identity.js's memory-custody mode
  // (bn-safari-sharedworker-cryptokey-dataclone), not by routing here.
  // Exactly one "port" exists — `self` itself — so there is only ever one
  // connectPort() call here, never a growing `ports` array the way a real
  // SharedWorker accumulates one per tab.
  connectPort(self);
}
