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
//
// bn-run-node-db-sync-wiring: this worker also owns the browser-replicated
// database lane (the "db lane" section below). When the admission capability
// carries a server-derived `db` block, it keeps the project's OPFS cr-sqlite
// replica converged with the fleet. Hosting constraint, measured live (Chrome,
// 2026-08): a SharedWorker global scope has NO `Worker` constructor at all
// (ReferenceError), and FileSystemSyncAccessHandle is DedicatedWorker-only —
// so in the SharedWorker context the sqlite worker is brokered by a connected
// page over this control protocol:
//   worker -> page : { type: "dbWorkerRequest", requestId }   (one page at a time)
//   page -> worker : { type: "dbWorkerPort", requestId } + transfer [port]
//     The page constructs
//       new Worker("<origin>/browser-node/sqlite/sqlite-worker.js", { type: "module" }),
//     bridges a MessageChannel to it both ways, and transfers the channel's
//     other end here. The page MUST terminate its bridge worker on
//     { type: "dbWorkerDone", requestId } (sent before this side drops the
//     port) and on its own unload — an orphaned sqlite worker holds the OPFS
//     access-handle pool and blocks the next open with
//     NoModificationAllowedError.
// In the dedicated-worker context (the Web Locks fallback) `Worker` exists and
// the lane nests directly — no page involvement.
// Page control ops added by the lane (replies on the originating port):
//   { type: "dbExec", id, sql }    write-class SQL; read_write grants only;
//                                  replies { type: "dbExecResult", id, ok, rows?|error? }
//   { type: "dbQuery", id, sql }   read-class SQL under any live grant; same reply
//   { type: "dbSyncNow", id }      runs a sync round now; replies
//                                  { type: "dbSyncResult", id, ok, reports?|error? }
//
// bn-browser-peer-webrtc-mesh: this worker also owns the browser↔browser
// DIRECT lane (the "mesh lane" section below). It is strictly ADDITIVE to the
// iroh relay path — see browser-node/peer-mesh.js for why it structurally
// cannot be a replacement — and every consumer degrades to the relay when a
// direct link is absent, failing, or simply not authorized.
//
// It needs TWO things this worker cannot provide alone:
//
// 1. A PAGE-HOSTED AGENT. `RTCPeerConnection` is `[Exposed=Window]`: it does
//    not exist in any worker global scope, so the peer connection is brokered
//    exactly like the sqlite worker above —
//      worker -> page : { type: "meshPeerRequest", requestId }   (one page at a time)
//      page  -> worker: { type: "meshPeerPort", requestId } + transfer [port]
//    The page does:
//      import { startMeshAgent } from "<origin>/browser-node/peer-mesh-agent.js";
//      const channel = new MessageChannel();
//      const agent = startMeshAgent(channel.port1);
//      workerPort.postMessage({ type: "meshPeerPort", requestId }, [channel.port2]);
//    and MUST call `agent.stop()` on { type: "meshPeerDone", requestId } and on
//    its own unload — an orphaned agent keeps PeerConnections alive after the
//    admission that authorized them is gone. A page that never answers is a
//    supported state: the lane stays dormant and everything uses the relay.
//
// 2. A FLEET SIGNALLING MAILBOX — the one server-side arm this feature needs,
//    owned by crates/hive-cloud. Exact contract this worker calls:
//
//      POST /cloud/v1/browser/signals      (header: x-hive-team)
//      body {
//        endpoint_id, challenge_ms, signature,   // the SAME proof-of-possession
//                                                // shape as POST /v1/browser/admissions
//        protocol_version: 1,
//        ack_seq: <u64>,                         // highest inbox seq consumed
//        send: [ { to: "<peer 64hex>", kind: "offer"|"answer"|"ice"|"bye",
//                  payload: "<JSON string, <= 16 KiB>" } ]   // <= 16 entries
//      }
//      200 { messages: [ { seq, from, kind, payload, sent_ms } ], retry_ms? }
//
//    Required server-side rules (each mirrors an invariant this codebase
//    already enforces elsewhere):
//      * POST, never GET. `admin_ingress` forwards mutations to the
//        control-plane leader but serves GETs LOCALLY behind round-robin DNS,
//        so a node-local mailbox read over GET reads the wrong node — the
//        failure class AGENTS.md documents. Reads ride this POST's reply.
//      * The caller must hold a live admission for `endpoint_id` under the
//        authenticated tenant, proven by the same PoP check `admit` runs.
//      * `to` must appear in the CALLER's own server-derived mesh peer set.
//        Anything else — unknown endpoint, foreign tenant, expired admission —
//        is the identical refusal, so a browser can neither enumerate peers nor
//        address a stranger.
//      * The mailbox is bounded per endpoint (drop-oldest; a suggested shape is
//        32 messages / 16 KiB each / 60s TTL) and consumed by `ack_seq`.
//      * A fleet without this arm answers 404/405/501, which this worker treats
//        as "no peer mesh yet", NOT as an error: the lane goes dormant and the
//        relay path is untouched.
//
//    And the capability half, on the SAME atomic admission response the
//    artifact/db blocks already ride (`capability.mesh`):
//      { enabled: true,
//        signal_poll_ms: 1500,
//        ice_servers: [ { urls: ["stun:...","turns:..."], username?, credential? } ],
//        peers: [ { endpoint_id, scope: "team"|"public", project,
//                   db: "read_write"|"read_only"|"none",
//                   artifacts: ["<policy_digest>", ...] } ] }
//    Every field SERVER-DERIVED from live admissions under the tenant, exactly
//    like `trusted_callers` and `db.sync_peers`: `db` must be "none" for a
//    Public-scope peer (an anonymous donor never writes tenant data) and
//    `artifacts` never wider than what the peer's own capability authorizes.
//    Absent block = no mesh, which is what every pre-upgrade node answers.
//
// 3. TWO DEPLOYMENT PREREQUISITES, both outside this file, both currently
//    UNMET — until each is done the lane is inert (status.mesh reports
//    "unavailable"/"predates peer-mesh signing" and every byte keeps riding
//    the relay, which is the designed degradation, not a crash):
//      * `ui/scripts/sync-browser-node.mjs` must list "peer-mesh.js" and
//        "peer-mesh-agent.js" alongside identity.js/artifact-policy.js.
//        It copies a hand-written allowlist, so an unlisted module is simply
//        never deployed and `import("./browser-node/peer-mesh.js")` 404s.
//      * `crates/hive-browser/build.sh` must be re-run and its www/pkg output
//        published: the signMeshEnvelope/verifyMeshEnvelope pair is new, and
//        without BOTH halves there is no way to bind a DTLS fingerprint to an
//        EndpointId — so the feature-detect below correctly refuses to open an
//        unauthenticated peer lane.

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
// v3 (browser-node-optional-serve-target): `start` now accepts an empty
// deployment/fn/digest ("attach to nothing"), and the admission capability can
// come back with `serving: false` and no artifact fields — a pre-v3 worker
// wedges on both. Keep in sync with ui/lib/run-node-status.ts.
// v4 (browser-auto-serve-eligible-set): `start` carries `serveMode`
// ("auto" | "none") and the capability answers with an ARRAY of authorized
// artifacts instead of one; a pre-v4 worker reads only the flat compat mirror
// and would serve one of them while the fleet routes all of them here.
const HOST_ABI_VERSION = 4;
// bn-p2p-version-negotiation (PWA wasm bundle): must match
// crates/hive-browser/src/lib.rs's wasm_bundle_version() return value --
// checked live against the ACTUAL loaded module right after init() succeeds
// (see start() below), not just trusted by filename/cache-key, so a stale
// service-worker-cached .wasm paired with fresh JS glue is caught even
// though the two are normally synced/cached together as a pair.
const WASM_BUNDLE_VERSION = 15; // v15: relay-online wait is non-fatal — boot returns a connecting node instead of throwing "relay online deadline exceeded"
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
let session = null; // { deployment, fn, digest, serveMode, scope, team, relay }

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
// browser-auto-serve-eligible-set: display context for the digests currently
// pinned (policyDigest -> { deployment, function, project }), straight from the
// server-derived capability. Status/UI only — nothing here is ever an
// authorization input: the invoke handler resolves on the pinned digest alone.
let fnRoutes = new Map();
// Per-artifact pin failures from the last reconcile ([{ digest, error }]).
// Surfaced honestly in status rather than silently dropped: one unreachable
// artifact must not blank a node that is serving the other nine, and must not
// look like it is serving something it is not.
let fnFailures = [];

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
  // browser-node-optional-serve-target (additive): is this node actually
  // serving a function right now? Derived from the pinned-artifact count in
  // updateFunctionsStatus, so it is a statement about real capability rather
  // than about what the user selected. A donor whose deployments are all
  // long-running servers runs perfectly well with this false — mesh, relay
  // identity, presence and database replication all work; only the serve lane
  // is idle.
  serving: false,
  // browser-auto-serve-eligible-set (additive): which serve lane this run
  // asked for — "auto" (whatever this tenant has, refreshed on every renewal),
  // "pinned" (one deliberately chosen function), or "none" (capacity only).
  // A statement about the REQUEST; `serving`/`functions` stay the statement
  // about what is actually pinned.
  serveMode: "none",
  // bn-run-node-db-sync-wiring introspection (additive on the wire, like
  // functions above; the ui/lib/run-node-status.ts typed mirror is owned
  // elsewhere): { project, dbFile, access, state, persisted, peers,
  // lastSyncMs, sites, siteVersion, error }. null unless the admitted
  // project's capability carries a server-derived `db` block — the dashboard
  // renders nothing then.
  db: null,
  // bn-browser-peer-webrtc-mesh introspection (additive, same discipline as
  // `db` above): PeerMesh.summary() — { state, known, connected, peers[],
  // direct, artifactHits, crrRounds, bytesIn/Out, error }. `connected` is a
  // live gauge; `direct` is the lifetime count of channels that actually
  // opened, which is what answers "did anything go direct at all". null unless the capability
  // carries a server-derived `mesh` block AND the lane got far enough to
  // exist. `state: "unsupported"` is an honest, expected rollout value, not an
  // error: it means this fleet has no signalling arm yet and every byte is
  // still riding the relay path exactly as before.
  mesh: null,
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

// Mid-rollout compatibility (browser-node-optional-serve-target): a node still
// running a pre-change binary rejects an admission carrying no serve target as
// a TERMINAL `function_target_invalid`. For a session that deliberately has no
// target, that verdict is about the node's age, not about this request — the
// public API host is round-robin DNS, so the next retry can land on an upgraded
// node and succeed. Downgraded to retryable so a donor never dead-ends on the
// first old node it happens to hit. A genuinely malformed target can't be
// confused with this: this worker only ever sends deployment+function together
// or neither, and this check requires the "neither" case.
function isPreUpgradeTargetlessRefusal(error) {
  if (!serveTargetless()) return false;
  const code = error && typeof error === "object" ? error.code : null;
  const message = String((error && error.message) || error || "");
  return code === "function_target_invalid" || message.includes("invalid browser function target");
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
      notePortRemoved(p); // the db lane re-brokers if this page held its sqlite worker
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

/** Re-mint the httpOnly `hive_jwt` cookie from inside the worker.
 *
 *  The dashboard mints on mount and then only every ~50 MINUTES
 *  (ui/components/session-token.tsx), while the backend's browser-admission
 *  bar requires a session no older than HIVE_BROWSER_SESSION_MAX_AGE_SECS —
 *  default 300 SECONDS (crates/hive-cloud/src/browser_admission.rs). So for
 *  roughly 45 of every 50 minutes an admission or renewal was rejected
 *  `session_stale`, and because that failure is classified RETRYABLE the
 *  worker just retried forever against the very same stale cookie: the node
 *  booted fine, never admitted, never published presence, and so never
 *  appeared on the constellation map. The page already self-heals exactly
 *  this way for its own requests (ui/lib/api.ts's mint-on-auth-failure);
 *  this is that same recovery for the worker, which had none.
 *
 *  Deduped through a single in-flight promise so a burst of renewals can't
 *  fire a mint storm at /api/token. */
let mintInFlight = null;
function remintSession(team) {
  if (!mintInFlight) {
    mintInFlight = (async () => {
      try {
        const r = await fetch("/api/token", {
          method: "POST",
          credentials: "same-origin",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ team }),
        });
        return r.ok;
      } catch {
        return false;
      }
    })().finally(() => {
      mintInFlight = null;
    });
  }
  return mintInFlight;
}

async function callApi(method, path, team, body) {
  try {
    return await callApiOnce(method, path, team, body);
  } catch (error) {
    // Only an auth-shaped failure is worth a mint; anything else propagates
    // untouched so the existing classify/backoff paths behave identically.
    const stale = Boolean(error) && (error.status === 401 || error.code === "session_stale");
    if (!stale) throw error;
    if (!(await remintSession(team))) throw error;
    // Exactly one retry: a second 401 after a SUCCESSFUL mint is a real
    // authorization answer (wrong tenant, revoked user), not a stale cookie.
    return await callApiOnce(method, path, team, body);
  }
}

async function callApiOnce(method, path, team, body) {
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

/** True while this session has no serve target at all
 *  (browser-node-optional-serve-target): "run a node" and "have a
 *  browser-servable function" are independent, so a donor whose deployments
 *  are all long-running servers/containers still joins the mesh, holds a relay
 *  identity, publishes presence, and (when a deployment with a `browser_db`
 *  block is attached) replicates its database. The serve lane simply stays
 *  idle — never faked. */
function serveTargetless(s = session) {
  return !s || !s.deployment || !s.fn;
}

async function admitOnce(addrJson, endpointId) {
  const { deployment, fn, digest, serveMode, scope, team } = session;
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
    // Both are OPTIONAL on the wire and empty means exactly what it says: no
    // deployment attached / no function served. The server decides the shape
    // (serving, database-only, or bare donor) — this side never asserts one.
    deployment: deployment || "",
    function: fn || "",
    digest: digest || "",
    // browser-auto-serve-eligible-set: a REQUEST for the automatic shape, not a
    // capability. With no deployment named, "auto" asks the server to derive
    // this tenant's whole eligible set itself; anything else (including an old
    // node that ignores the field entirely) leaves the serve lane empty. This
    // side never names what lands in the set.
    serve_mode: serveMode === "auto" ? "auto" : "none",
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

// The worker's own ceiling on how many artifacts one admission may pin
// (browser-auto-serve-eligible-set). The server bounds its eligible set well
// below this (HIVE_BROWSER_AUTO_SERVE_MAX, default 16); this is the donor's
// independent refusal to let a server-side bug turn one tab into an unbounded
// pin set. Over the cap the extras are DROPPED and named in status — never
// silently, and never by throwing away an otherwise-valid admission.
const MAX_PINNED_ARTIFACTS = 32;

// Bounded display label from the capability. Server-derived, rendered as text
// by the dashboard, never used as an identifier here.
function label(value) {
  return typeof value === "string" ? value.slice(0, 128) : "";
}

// Validate ONE capability artifact entry into a local descriptor. Everything
// not matching is a hard error — a malformed entry pins nothing.
function normalizeArtifact(entry) {
  const descriptor = {
    policyDigest: entry.policy_digest,
    sourceDigest: entry.source_digest,
    sourceBytes: entry.source_bytes,
    mode: entry.mode,
    timeoutMs: entry.timeout_ms,
    memoryBytes: entry.memory_bytes,
    stackBytes: entry.stack_bytes,
    allowedOps: entry.allowed_ops,
    deployment: label(entry.deployment),
    function: label(entry.function),
    project: label(entry.project),
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
  if (entry.artifact_url !== expectedUrl) {
    throw new Error("capability artifact_url does not match its policy digest");
  }
  return descriptor;
}

// Validate the capability block's shape into the local descriptor SET
// (browser-auto-serve-eligible-set). Everything not matching is a hard error —
// a malformed capability pins nothing.
//
// A capability with `serving: false` carries NO artifact fields and an empty
// trusted-caller list (browser-node-optional-serve-target). That is a
// well-formed answer, not a malformed one: it means the server authorized this
// browser as a node without authorizing it to serve anything. It resolves to an
// EMPTY descriptor set with no callers, so the reconcile below revokes whatever
// the previous capability granted and grants nothing — absence of capability is
// never the same as a broader one.
//
// Rollout: a node that predates `artifacts[]` answers with the flat single
// artifact fields, which are read as a one-element set. The server ALSO mirrors
// its first entry into those same flat fields, so neither direction of the
// rollout ever sees a shape it cannot parse.
function normalizeCapability(capability) {
  if (!capability || typeof capability !== "object") {
    throw new Error("admission response is missing its capability block");
  }
  // trusted_callers is server-derived (healthy fleet EndpointIds); filter to
  // the exact 64-hex shape anyway — grantInvoker accepts nothing else, and a
  // malformed entry must never reach the grant map.
  const trustedCallers = Array.isArray(capability.trusted_callers) ? capability.trusted_callers : [];
  const callers = [...new Set(trustedCallers.filter(id => typeof id === "string" && DIGEST_RE.test(id)))];
  if (capability.serving === false) {
    return { descriptors: [], callers: [], dropped: 0 };
  }
  const entries = Array.isArray(capability.artifacts) ? capability.artifacts : [capability];
  const descriptors = [];
  const seen = new Set();
  let dropped = 0;
  for (const entry of entries) {
    if (!entry || typeof entry !== "object") throw new Error("capability artifact entry is not an object");
    const descriptor = normalizeArtifact(entry);
    // Two deployments can legitimately reference the SAME content-addressed
    // artifact; one pin serves every route that names its digest.
    if (seen.has(descriptor.policyDigest)) continue;
    if (descriptors.length >= MAX_PINNED_ARTIFACTS) {
      dropped += 1;
      continue;
    }
    seen.add(descriptor.policyDigest);
    descriptors.push(descriptor);
  }
  return { descriptors, callers, dropped };
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

// Pin ONE descriptor (cache, then an authorized browser peer, then the
// authenticated content-addressed GET), verifying both digests locally before
// anything is executable. Returns true once the digest is pinned; throws on any
// verification/fetch failure.
//
// The PEER hop (bn-browser-peer-webrtc-mesh) is safe for exactly one reason,
// and it is the same reason the cache hop is: `runtime.pin` recomputes the
// size and the BLAKE3 source digest and the canonical policy digest from the
// bytes themselves, so a peer cannot launder anything into this runtime — the
// worst it can do is waste a round trip and fall through to the fleet. It is
// never a dependency: no peer, no link, wrong bytes and a refused request all
// take the identical fleet path below.
async function pinArtifact(runtime, descriptor, team, owner, myEpoch) {
  if (runtime.has(descriptor.policyDigest)) return true;
  let bytes = await readCachedArtifact(descriptor);
  if (bytes) {
    try {
      runtime.pin(descriptor, bytes);
      return true;
    } catch {
      await dropCachedArtifact(descriptor.policyDigest);
      bytes = null;
    }
  }
  const peerBytes = await fetchArtifactFromMesh(descriptor);
  if (peerBytes) {
    if (myEpoch !== epoch || node !== owner) return false;
    try {
      runtime.pin(descriptor, peerBytes);
      await persistVerifiedArtifact(descriptor, peerBytes);
      return true;
    } catch {
      /* a peer's bytes failed local verification — fall through to the fleet */
    }
  }
  bytes = await fetchArtifactBytes(descriptor, team);
  if (myEpoch !== epoch || node !== owner) return false; // a newer epoch owns teardown now
  runtime.pin(descriptor, bytes); // throws on any digest/size mismatch
  await persistVerifiedArtifact(descriptor, bytes);
  return true;
}

// One atomic reconcile per admission/renewal response, over the whole
// authorized SET (browser-auto-serve-eligible-set). Verification fully precedes
// mutation: every artifact is fetched + pinned (both digests recomputed
// locally) BEFORE any grant changes, then stale digests/callers are revoked
// BEFORE their replacements are granted.
//
// A per-artifact failure does NOT discard the rest: with a set, one unreachable
// artifact would otherwise take down a node serving every other one. The failed
// digest is simply never granted (nothing can be invoked that is not pinned),
// it is named in status, and the next renewal retries it. Only a TOTAL failure
// — every authorized artifact failed — throws, preserving exactly today's
// backoff-and-retry behaviour for the single-artifact case.
async function reconcileCapability(capability, myEpoch) {
  const runtime = fnRuntime;
  const owner = node;
  if (!runtime || runtime.closed || !owner) throw new Error("function runtime is not running");
  const team = session && session.team;
  const { descriptors, callers, dropped } = normalizeCapability(capability);

  const pinned = [];
  const failures = [];
  for (const descriptor of descriptors) {
    try {
      if (await pinArtifact(runtime, descriptor, team, owner, myEpoch)) pinned.push(descriptor);
    } catch (error) {
      failures.push({
        digest: descriptor.policyDigest,
        error: String((error && error.message) || error).slice(0, 200),
      });
    }
    if (myEpoch !== epoch || node !== owner) return;
  }
  if (descriptors.length > 0 && pinned.length === 0) {
    fnFailures = failures;
    throw new Error(failures[0] ? failures[0].error : "no authorized artifact could be pinned");
  }
  if (myEpoch !== epoch || node !== owner) return;

  const desiredDigests = new Set(pinned.map((d) => d.policyDigest));
  // Revoke stale digests (close their runners) and stale callers FIRST. This
  // covers the serve-less admission too: an empty authorized set revokes every
  // grant and unpins everything, and the invoke handler — which resolves
  // exclusively against pinned digests — then rejects every invoke. The node is
  // running and contributing, and serving nothing.
  for (const [digest, granted] of [...fnGrants]) {
    if (desiredDigests.has(digest)) continue;
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
  // An artifact pinned but never granted (a previous reconcile lost its epoch
  // race, or its grant set was empty) is still executable-in-principle memory:
  // drop it too once it leaves the authorized set.
  for (const digest of runtime.stats().pinned) {
    if (!desiredDigests.has(digest)) runtime.unpin(digest);
  }

  const desired = new Set(callers);
  for (const descriptor of pinned) {
    const digest = descriptor.policyDigest;
    const current = fnGrants.get(digest) || new Set();
    for (const caller of [...current]) {
      if (desired.has(caller)) continue;
      try {
        owner.revokeInvoker(caller, digest);
      } catch {
        /* node already closed */
      }
      current.delete(caller);
    }
    // Only now grant replacements: every exact returned caller EndpointId, for
    // only the returned policy digests.
    for (const caller of desired) {
      if (current.has(caller)) continue;
      owner.grantInvoker(caller, digest);
      current.add(caller);
    }
    fnGrants.set(digest, current);
  }
  fnRoutes = new Map(
    pinned.map((d) => [d.policyDigest, { deployment: d.deployment, function: d.function, project: d.project }]),
  );
  fnFailures = failures;
  if (dropped > 0) {
    fnFailures = [
      ...failures,
      { digest: "", error: `${dropped} more authorized artifact${dropped === 1 ? "" : "s"} exceeded this browser's ${MAX_PINNED_ARTIFACTS}-artifact cap and are not served here` },
    ];
  }
  updateFunctionsStatus();
}

function updateFunctionsStatus() {
  if (!fnRuntime || fnRuntime.closed) {
    if (status.functions !== null || status.serving !== false) {
      setStatus({ functions: null, serving: false });
    }
    return;
  }
  const stats = fnRuntime.stats();
  let grants = 0;
  for (const callers of fnGrants.values()) grants += callers.size;
  // `serving` is DERIVED from what is actually pinned, never from what the
  // session asked for (browser-node-optional-serve-target): a target-less or
  // database-only node pins nothing, so it reports false and the dashboard can
  // say plainly that it is running without serving a function.
  // `stats.pinned` is the ARRAY of pinned digests (WorkerFunctionRuntime.stats
  // returns `[...artifacts.keys()]`), so this must test its LENGTH — `array > 0`
  // stringifies and compares as NaN, which is false for a genuinely serving
  // node too, i.e. silently always-false.
  setStatus({
    functions: {
      pinned: stats.pinned,
      grants,
      served: stats.servedTotal,
      // browser-auto-serve-eligible-set: what those digests ARE, so the
      // dashboard can name the functions this node is carrying instead of
      // showing a count. Ordered by the capability, filtered to what is
      // actually pinned right now.
      serving: stats.pinned.map((digest) => ({ digest, ...(fnRoutes.get(digest) || {}) })),
      failed: fnFailures,
    },
    serving: stats.pinned.length > 0,
  });
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
  fnRoutes = new Map();
  fnFailures = [];
  if (fnRuntime) {
    try {
      fnRuntime.close();
    } catch {
      /* already closed */
    }
    fnRuntime = null;
  }
}

// ---------------------------------------------------------------------------
// bn-run-node-db-sync-wiring: the browser-replicated database lane.
//
// When the admission capability carries a server-derived `db` block
// (docs/browser-db-contract.md §3), this worker keeps the project's OPFS
// cr-sqlite replica converged with the fleet: it spawns (or brokers — see the
// file header) the sqlite DedicatedWorker from the deployed assets, opens the
// replica persist-first, applies the spec schema idempotently, serves
// fleet-initiated rounds through the node's setCrrSyncHandler arm, and runs
// the BrowserDbSync session loop against the granted sync_peers on a fixed
// cadence plus after every local write. The lane NEVER fails an admission:
// every failure lands in status.db.error instead (the function lane is the
// admission's purpose; the database is additive).
//
// Retention (contract §5), keyed on the ADMISSION, never on a sync refusal:
//   * stop() — the local revoke: best-effort final push while the grant is
//     still live, then wipe the OPFS replica.
//   * terminal admission failure (denied, e.g. deployment_not_ready) → wipe.
//   * a successful renewal whose capability LOST the db block → grant gone →
//     wipe.
//   * a refused sync round (relay denylist / expired lease) → SEAL the lane
//     and kick the renewal spine: a fresh admission resumes incrementally
//     from the replica's durable watermarks; renewals that keep failing leave
//     it sealed. A sync refusal alone NEVER wipes — a transient dial failure
//     is indistinguishable from revocation at that layer.
// ---------------------------------------------------------------------------

const DB_SYNC_INTERVAL_MS = 30_000; // a converged round is one small frame per peer
const DB_WORKER_BROKER_TIMEOUT_MS = 6_000; // per page asked, sequentially
const DB_RESPAWN_BASE_MS = 5_000;
const DB_RESPAWN_CAP_MS = 60_000;
const DB_WORKER_URL = new URL("./browser-node/sqlite/sqlite-worker.js", import.meta.url);

let dbLane = null;
// shape: {
//   capability, client, sync, responder, makeCrrSyncResponder,
//   endpoint, nested, brokerPort, brokerRequestId, requestSeq, pendingBroker,
//   grantedPeers, state ("opening"|"idle"|"syncing"|"error"), sealed,
//   persisted, lastSyncMs, sites, siteVersion, error,
//   syncTimer, syncInFlight, syncKickPending, syncWaiters,
//   respawnTimer, respawnDelayMs, schemaKey, openSettled, wipeOnOpen,
// }

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Validate the capability's `db` block into a local shape. Absent = null (the
// project is not opted in — normal). Malformed = thrown, landing in
// status.db.error: the block is server-derived, so a bad shape is a platform
// bug to surface, never client input to sanitize silently.
function normalizeDbCapability(capability) {
  const db = capability && capability.db;
  if (db === undefined || db === null) return null;
  if (typeof db !== "object") throw new Error("capability db block is not an object");
  // The platform-derived replica name (contract §6) — a bare file name from
  // the platform template, never a path; it becomes an OPFS file name below.
  if (typeof db.db_file !== "string" || !/^hive-browserdb-[a-z0-9._-]+\.db$/.test(db.db_file)) {
    throw new Error("capability db.db_file is not the platform-derived replica name");
  }
  if (db.access !== "read_write" && db.access !== "read_only") {
    throw new Error(`capability db.access ${JSON.stringify(db.access)} is unsupported`);
  }
  if (typeof db.project !== "string" || !db.project) {
    throw new Error("capability db.project must be a non-empty string");
  }
  for (const key of ["max_bytes", "max_value_bytes"]) {
    if (!Number.isSafeInteger(db[key]) || db[key] <= 0) {
      throw new Error(`capability db.${key} must be a positive integer`);
    }
  }
  const schema = Array.isArray(db.schema) ? db.schema : [];
  for (const table of schema) {
    if (!table || typeof table !== "object" || typeof table.ddl !== "string" || !/^[A-Za-z_][A-Za-z0-9_]*$/.test(table.name || "")) {
      throw new Error("capability db.schema carries a malformed table entry");
    }
  }
  // sync_peers is server-derived from the live registry (health-filtered
  // fleet EndpointIds); filter to the exact shape anyway — a malformed entry
  // must never reach grantCrrSync/crrSyncOn.
  const peers = (Array.isArray(db.sync_peers) ? db.sync_peers : [])
    .filter((p) => p && typeof p === "object" && DIGEST_RE.test(p.endpoint_id || "") && typeof p.addr_json === "string" && p.addr_json.length > 0 && p.addr_json.length <= 8192)
    .map((p) => ({ endpoint_id: p.endpoint_id, addr_json: p.addr_json }));
  return {
    tenant: typeof db.tenant === "string" ? db.tenant : "",
    project: db.project,
    access: db.access,
    max_bytes: db.max_bytes,
    max_value_bytes: db.max_value_bytes,
    db_file: db.db_file,
    schema,
    sync_peers: peers,
    expires_ms: Number.isSafeInteger(db.expires_ms) ? db.expires_ms : 0,
  };
}

function dbSchemaKey(dbCap) {
  return JSON.stringify([dbCap.schema, dbCap.max_bytes, dbCap.max_value_bytes, dbCap.access]);
}

// One status object, rebuilt from the lane on every transition; null while no
// lane exists (a project without a browser_db block never sets it, and the
// dashboard renders nothing). The JSON compare avoids a broadcast per
// transition when nothing actually changed.
function updateDbStatus() {
  const summary = dbLane
    ? {
        project: dbLane.capability.project,
        dbFile: dbLane.capability.db_file,
        access: dbLane.capability.access,
        state: dbLane.sealed && dbLane.state !== "error" ? "sealed" : dbLane.state,
        persisted: dbLane.persisted,
        peers: dbLane.capability.sync_peers.length,
        lastSyncMs: dbLane.lastSyncMs,
        sites: dbLane.sites,
        siteVersion: dbLane.siteVersion,
        error: dbLane.error,
      }
    : null;
  if (JSON.stringify(status.db ?? null) === JSON.stringify(summary)) return;
  setStatus({ db: summary });
}

// A db-lane failure when no lane exists yet (a malformed db block, a spawn
// that never got far enough): still surfaced, never hidden — but never
// failing the admission itself.
function noteDbLaneError(error) {
  const message = String((error && error.message) || error).slice(0, 300);
  if (dbLane) {
    dbLane.state = "error";
    dbLane.error = message;
    updateDbStatus();
  } else {
    setStatus({
      db: {
        project: null, dbFile: null, access: null, state: "error", persisted: null,
        peers: 0, lastSyncMs: null, sites: 0, siteVersion: 0, error: message,
      },
    });
  }
}

// Spawn-or-broker the sqlite DedicatedWorker endpoint. Measured live (Chrome,
// 2026-08): a SharedWorker global scope has NO `Worker` constructor at all
// (ReferenceError: Worker is not defined) while a dedicated worker nests
// fine — feature detection, not context sniffing.
async function acquireDbWorkerEndpoint(lane, myEpoch) {
  if (typeof Worker === "function") {
    const worker = new Worker(DB_WORKER_URL, { type: "module" });
    lane.nested = true;
    return worker;
  }
  // SharedWorker: broker through connected pages, one at a time, visible tabs
  // first — never a broadcast (every answering page builds a real sqlite
  // worker; sequential asks leave at most one bridge alive).
  let lastError = null;
  const candidates = [...ports].sort(
    (a, b) => Number(portVisibility.get(b) ?? true) - Number(portVisibility.get(a) ?? true),
  );
  for (const port of candidates) {
    if (myEpoch !== epoch || dbLane !== lane) throw new Error("db lane superseded during worker broker");
    try {
      // onDbWorkerPort records the answering port as lane.brokerPort.
      return await requestBrokeredDbWorker(lane, port);
    } catch (error) {
      lastError = error;
    }
  }
  throw lastError || new Error("no connected page could broker the database worker");
}

function requestBrokeredDbWorker(lane, port) {
  return new Promise((resolve, reject) => {
    const requestId = ++lane.requestSeq;
    const timer = setTimeout(() => {
      if (lane.pendingBroker && lane.pendingBroker.requestId === requestId) lane.pendingBroker = null;
      reject(new Error("page did not broker the database worker in time"));
    }, DB_WORKER_BROKER_TIMEOUT_MS);
    lane.pendingBroker = {
      requestId,
      resolve: (endpoint) => { clearTimeout(timer); resolve(endpoint); },
      reject: (error) => { clearTimeout(timer); reject(error); },
    };
    try {
      port.postMessage({ type: "dbWorkerRequest", requestId });
    } catch (error) {
      clearTimeout(timer);
      if (lane.pendingBroker && lane.pendingBroker.requestId === requestId) lane.pendingBroker = null;
      reject(error);
    }
  });
}

function onDbWorkerPort(port, msg, transferred) {
  const lane = dbLane;
  const endpoint = transferred && transferred[0];
  const pending = lane && lane.pendingBroker;
  if (!lane || !pending || msg.requestId !== pending.requestId) {
    // Stale or unsolicited answer — close the end so neither side dangles.
    try {
      if (endpoint) endpoint.close();
    } catch {
      /* ignore */
    }
    return;
  }
  lane.pendingBroker = null;
  if (!endpoint) {
    pending.reject(new Error("dbWorkerPort arrived without a transferred port"));
    return;
  }
  if (typeof endpoint.start === "function") endpoint.start();
  lane.brokerPort = port;
  lane.brokerRequestId = msg.requestId;
  pending.resolve(endpoint);
}

function teardownDbEndpoint(lane) {
  if (lane.syncTimer) {
    clearInterval(lane.syncTimer);
    lane.syncTimer = null;
  }
  if (lane.respawnTimer) {
    clearTimeout(lane.respawnTimer);
    lane.respawnTimer = null;
  }
  if (lane.pendingBroker) {
    lane.pendingBroker.reject(new Error("db lane torn down"));
    lane.pendingBroker = null;
  }
  if (!lane.nested && lane.brokerPort) {
    // Tell the brokering page to terminate its sqlite worker NOW — an orphan
    // holds the OPFS access-handle pool and blocks the next open.
    try {
      lane.brokerPort.postMessage({ type: "dbWorkerDone", requestId: lane.brokerRequestId });
    } catch {
      /* page already gone */
    }
  }
  lane.brokerPort = null;
  if (lane.endpoint) {
    if (lane.nested) {
      try {
        lane.endpoint.terminate();
      } catch {
        /* ignore */
      }
    } else {
      try {
        lane.endpoint.close();
      } catch {
        /* ignore */
      }
    }
    lane.endpoint = null;
  }
  lane.client = null;
  lane.sync = null;
  lane.responder = null;
}

function reconcileDbPeerGrants(owner, lane) {
  const desired = new Set(lane.capability.sync_peers.map((p) => p.endpoint_id));
  for (const id of lane.grantedPeers) {
    if (desired.has(id)) continue;
    try {
      owner.revokeCrrSync(id);
    } catch {
      /* node already closed */
    }
  }
  for (const id of desired) {
    try {
      owner.grantCrrSync(id);
    } catch {
      /* node already closed */
    }
  }
  lane.grantedPeers = desired;
}

function revokeDbPeerGrants(owner, lane) {
  for (const id of lane.grantedPeers) {
    try {
      owner.revokeCrrSync(id);
    } catch {
      /* node already closed */
    }
  }
  lane.grantedPeers = new Set();
}

async function startDbLane(dbCap, myEpoch) {
  const owner = node;
  if (!owner) return;
  const lane = {
    capability: dbCap, client: null, sync: null, responder: null, makeCrrSyncResponder: null,
    endpoint: null, nested: false, brokerPort: null, brokerRequestId: 0,
    requestSeq: 0, pendingBroker: null, grantedPeers: new Set(),
    state: "opening", sealed: false, persisted: null,
    lastSyncMs: null, sites: 0, siteVersion: 0, error: null,
    syncTimer: null, syncInFlight: false, syncKickPending: false, syncWaiters: [],
    respawnTimer: null, respawnDelayMs: DB_RESPAWN_BASE_MS, schemaKey: dbSchemaKey(dbCap),
    openSettled: null, wipeOnOpen: false,
  };
  dbLane = lane;
  updateDbStatus();
  try {
    // Dynamic import: a missing or partial deployed sqlite set must degrade
    // the db lane honestly, never break the whole run-node worker.
    const syncClient = await import("./browser-node/sqlite/sync-client.js");
    lane.makeCrrSyncResponder = syncClient.makeCrrSyncResponder;
    lane.endpoint = await acquireDbWorkerEndpoint(lane, myEpoch);
    if (myEpoch !== epoch || node !== owner || dbLane !== lane) {
      teardownDbEndpoint(lane);
      return;
    }
    lane.client = new syncClient.WorkerClient(lane.endpoint);
    lane.sync = new syncClient.BrowserDbSync(lane.client, owner, dbCap);
    // A just-closed previous worker's OPFS sync access handles drain
    // asynchronously (one pool per origin owns /hive-crsql at a time) — ride
    // out the release window, but only for that exact error (the
    // witness-crr.html precedent).
    const openAttempt = (async () => {
      for (let attempt = 1; attempt <= 6; attempt++) {
        try {
          return await lane.sync.open();
        } catch (error) {
          if (!/NoModificationAllowedError/.test(String((error && error.message) || error)) || attempt === 6) throw error;
          await sleep(1_000);
        }
      }
      throw new Error("unreachable");
    })();
    lane.openSettled = openAttempt.catch(() => {}); // never rejects: stopDbLane only waits on it
    const opened = await openAttempt;
    if (myEpoch !== epoch || node !== owner || dbLane !== lane) {
      // stop() landed mid-open — the replica now exists; honor the wipe the
      // stop requested before releasing the endpoint.
      if (lane.wipeOnOpen) {
        try {
          await lane.client.call("wipe");
        } catch {
          /* best effort */
        }
      }
      teardownDbEndpoint(lane);
      return;
    }
    lane.persisted = opened && opened.persisted !== undefined ? opened.persisted : "unknown";
    lane.responder = lane.makeCrrSyncResponder(lane.client, dbCap);
    owner.setCrrSyncHandler((bytes, signal) => {
      if (dbLane !== lane || !lane.responder) {
        return Promise.reject(new Error("this browser node holds no live db grant"));
      }
      return lane.responder(bytes, signal);
    });
    reconcileDbPeerGrants(owner, lane);
    lane.state = "idle";
    lane.respawnDelayMs = DB_RESPAWN_BASE_MS;
    updateDbStatus();
    lane.syncTimer = setInterval(() => runDbSync(lane), DB_SYNC_INTERVAL_MS);
    runDbSync(lane); // the initial convergence round — async, reports into status
  } catch (error) {
    if (dbLane !== lane) {
      teardownDbEndpoint(lane);
      return;
    }
    lane.state = "error";
    lane.error = String((error && error.message) || error).slice(0, 300);
    updateDbStatus();
    scheduleDbRespawn(lane);
  }
}

// Rebuild the sqlite worker endpoint (brokered page died, nested worker
// crashed, open failed) WITHOUT touching the replica: the OPFS file and its
// durable watermarks survive, so the respawned lane resumes incrementally.
function scheduleDbRespawn(lane, delayMs) {
  if (dbLane !== lane) return;
  if (lane.respawnTimer) clearTimeout(lane.respawnTimer);
  const delay = delayMs ?? lane.respawnDelayMs;
  lane.respawnDelayMs = Math.min(lane.respawnDelayMs * 2, DB_RESPAWN_CAP_MS);
  const myEpoch = epoch;
  const capability = lane.capability;
  lane.respawnTimer = setTimeout(() => {
    if (dbLane !== lane || myEpoch !== epoch || !node) return;
    teardownDbEndpoint(lane);
    if (dbLane === lane) dbLane = null;
    startDbLane(capability, myEpoch);
  }, delay);
}

// Expiry/offline semantics (contract §5): the replica is SEALED — no exchange
// until a successful renewal re-grants. Never wiped on these paths.
function sealDbLane() {
  const lane = dbLane;
  if (!lane) return;
  lane.sealed = true;
  updateDbStatus();
}

// Every lane teardown funnels here: stop() (local revoke — final push, then
// wipe), terminal admission failure / a lost db block (wipe, no push — the
// grant is already gone server-side), and the defensive epoch clear (neither).
// `owner` is the node the peer grants were applied to.
async function stopDbLane({ wipe, finalPush, owner = node }) {
  const lane = dbLane;
  if (!lane) return;
  dbLane = null;
  updateDbStatus();
  if (owner) revokeDbPeerGrants(owner, lane);
  // The inbound responder arm rejects from here on (its setCrrSyncHandler
  // closure checks dbLane !== lane); nothing more to uninstall.
  if (finalPush && lane.sync && lane.client && networkOnline) {
    try {
      await lane.sync.syncAll();
    } catch {
      /* best-effort final push — the wipe below is unconditional */
    }
  }
  if (wipe) {
    if (!lane.client && lane.state === "opening") {
      // stop() raced the initial open: give it a bounded moment to settle so
      // the replica it may have just created can still be wiped here; the
      // post-open stale branch in startDbLane honors wipeOnOpen otherwise.
      lane.wipeOnOpen = true;
      await Promise.race([lane.openSettled || Promise.resolve(), sleep(3_000)]);
    }
    if (lane.client) {
      try {
        await lane.client.call("wipe");
      } catch {
        /* the replica may already be gone */
      }
    }
  }
  teardownDbEndpoint(lane);
}

// One reconcile per successful admission/renewal, after the function lane's
// own reconcile: start on a fresh grant, adopt the renewed capability on an
// existing lane, wipe on a lost one. A db-lane failure is caught by the
// caller (renewNow) into status.db — never an admission failure.
async function reconcileDbLane(capability, myEpoch) {
  const dbCap = normalizeDbCapability(capability);
  if (!dbCap) {
    if (dbLane) await stopDbLane({ wipe: true, finalPush: false });
    else if (status.db) setStatus({ db: null });
    return;
  }
  if (dbLane && dbLane.capability.db_file !== dbCap.db_file) {
    // A different project's grant replaced this one within one admission
    // lifetime — the old project's grant is gone, so its replica is wiped
    // before the new one opens (§5).
    await stopDbLane({ wipe: true, finalPush: false });
  }
  if (!dbLane) {
    await startDbLane(dbCap, myEpoch);
    return;
  }
  // Same database, renewed grant: adopt the fresh capability (caps/schema/
  // peers may have moved across a redeploy), reconcile peer grants, unseal.
  const lane = dbLane;
  const wasSealed = lane.sealed;
  lane.sealed = false;
  lane.capability = dbCap;
  if (lane.sync) lane.sync.cap = dbCap;
  if (lane.client && lane.makeCrrSyncResponder) {
    lane.responder = lane.makeCrrSyncResponder(lane.client, dbCap);
  }
  const owner = node;
  if (owner) reconcileDbPeerGrants(owner, lane);
  if (lane.schemaKey !== dbSchemaKey(dbCap) && lane.client) {
    lane.schemaKey = dbSchemaKey(dbCap);
    // Idempotent by contract (§1): CREATE TABLE IF NOT EXISTS + crsql_as_crr.
    for (const table of dbCap.schema) {
      await lane.client.call("exec", { sql: table.ddl });
      await lane.client.call("as-crr", { table: table.name });
    }
  }
  updateDbStatus();
  if (wasSealed) kickDbSync(); // re-admitted: resume from the durable watermarks
}

function kickDbSync() {
  const lane = dbLane;
  if (!lane) return;
  if (lane.syncInFlight) {
    lane.syncKickPending = true;
    return;
  }
  runDbSync(lane);
}

// One coalesced sync pass over the granted peers. Waiters (the dbSyncNow
// control op) receive the raw per-peer session reports; the periodic caller
// ignores them.
async function runDbSync(lane) {
  if (dbLane !== lane || !lane.sync) return null;
  if (lane.sealed || !networkOnline) return null;
  if (lane.syncInFlight) {
    lane.syncKickPending = true;
    return null;
  }
  lane.syncInFlight = true;
  lane.state = "syncing";
  updateDbStatus();
  let reports = null;
  try {
    // Fleet peers FIRST, then whatever direct browser links exist right now
    // (bn-browser-peer-webrtc-mesh). Convergence never depends on the direct
    // half: with no link the list is exactly the fleet set and this is the
    // pre-change pass. With one, the same watermark protocol runs over the
    // DataChannel and the fleet round that follows sees the merged state.
    reports = await lane.sync.syncAll([...(lane.capability.sync_peers ?? []), ...meshDbPeers()]);
    if (dbLane !== lane) return reports;
    // Only a FLEET refusal is the revocation/expiry signal. A direct peer's
    // refusal means its tab closed or its channel died — sealing on that would
    // stop a perfectly valid fleet grant for a reason the fleet never gave.
    if (reports.some((r) => r.forbidden && !r.direct)) {
      // The fleet refused a round: the grant is revoked or the lease lapsed.
      // SEAL and let the renewal spine's re-admission verdict decide
      // resume-vs-stay-sealed (contract §5) — a sync refusal alone never
      // wipes; a transient dial failure looks identical at this layer.
      lane.error = "the fleet refused a sync round — the db grant was revoked or the lease lapsed; sealed until re-admission";
      lane.state = "idle";
      sealDbLane();
      kickRenew();
      return reports;
    }
    lane.lastSyncMs = Date.now();
    lane.error = null;
    const state = await lane.sync.state();
    if (dbLane !== lane) return reports;
    lane.sites = state.watermarks.length;
    const own = state.watermarks.find((w) => w.siteId === state.siteId);
    lane.siteVersion = own ? own.version : 0;
    lane.state = "idle";
    updateDbStatus();
    return reports;
  } catch (error) {
    if (dbLane !== lane) return reports;
    lane.state = "error";
    lane.error = String((error && error.message) || error).slice(0, 300);
    updateDbStatus();
    // A thrown op (e.g. WorkerClient's 30s timeout) means the endpoint is
    // likely dead — a brokered page vanished or the nested worker crashed.
    scheduleDbRespawn(lane);
    return reports;
  } finally {
    if (dbLane === lane) {
      lane.syncInFlight = false;
      const waiters = lane.syncWaiters;
      lane.syncWaiters = [];
      for (const resolve of waiters) resolve(reports);
      if (lane.syncKickPending) {
        lane.syncKickPending = false;
        runDbSync(lane);
      }
    }
  }
}

// The page that brokered the current sqlite worker is gone — its
// DedicatedWorker died with it. Re-broker from a surviving page (the OPFS
// replica and its watermarks are unaffected; the open retry rides out the
// handle drain). The peer-mesh agent is brokered from a page too and dies the
// same way, so both lanes are notified from this one call site.
function notePortRemoved(port) {
  noteMeshPortRemoved(port);
  const lane = dbLane;
  if (!lane) return;
  if (lane.pendingBroker) {
    try {
      lane.pendingBroker.reject(new Error("brokering page disconnected"));
    } catch {
      /* already settled */
    }
    lane.pendingBroker = null;
  }
  if (lane.brokerPort !== port) return;
  lane.brokerPort = null;
  if (lane.endpoint) {
    try {
      lane.endpoint.close();
    } catch {
      /* already dead with the page */
    }
    lane.endpoint = null;
  }
  if (lane.state !== "error") {
    lane.state = "opening";
    updateDbStatus();
  }
  scheduleDbRespawn(lane, 1_500);
}

// Page control ops. dbExec is write-class (read_write grants only) and syncs
// on write (contract §5's unsynced-writes mitigation); dbQuery is read-class
// under any live grant. Both reply on the originating port.
async function handleDbExec(port, msg, writeClass) {
  const reply = (ok, fields) => {
    try {
      port.postMessage({ type: "dbExecResult", id: msg.id, ok, ...fields });
    } catch {
      /* tab gone */
    }
  };
  const lane = dbLane;
  if (!lane || !lane.sync || lane.state === "opening") {
    return reply(false, { error: "no live database lane — this target has no browser_db grant, or it is still opening" });
  }
  if (lane.sealed) {
    return reply(false, { error: "the database grant is sealed (admission not currently granted) — it resumes on re-admission" });
  }
  if (writeClass && lane.capability.access !== "read_write") {
    return reply(false, { error: "the database grant is read-only" });
  }
  try {
    if (writeClass) {
      const r = await lane.sync.write(String(msg.sql));
      kickDbSync();
      reply(true, { rows: r.rows ?? [] });
    } else {
      const rows = await lane.sync.readRows(String(msg.sql));
      reply(true, { rows });
    }
  } catch (error) {
    reply(false, { error: String((error && error.message) || error).slice(0, 300) });
  }
}

async function handleDbSyncNow(port, msg) {
  const id = msg.id;
  const reply = (ok, fields) => {
    try {
      port.postMessage({ type: "dbSyncResult", id, ok, ...fields });
    } catch {
      /* tab gone */
    }
  };
  const lane = dbLane;
  if (!lane || !lane.sync || lane.sealed) {
    return reply(false, { error: lane && lane.sealed ? "the database grant is sealed" : "no live database lane" });
  }
  const reportsPromise = new Promise((resolve) => lane.syncWaiters.push(resolve));
  kickDbSync();
  const reports = await Promise.race([reportsPromise, sleep(60_000).then(() => "timeout")]);
  if (reports === "timeout") return reply(false, { error: "sync round did not finish within 60s" });
  reply(true, { reports });
}

// ---------------------------------------------------------------------------
// bn-browser-peer-webrtc-mesh: the browser↔browser direct lane.
//
// Additive to the relay path in every direction (see the file header and
// browser-node/peer-mesh.js). This section owns the wiring only: the
// authenticated signalling round trip, the page-agent broker, and the two
// handlers that decide what a peer may actually pull from THIS node. All
// peer-connection mechanics live in peer-mesh.js (worker half) and
// peer-mesh-agent.js (page half).
//
// The lane NEVER fails an admission — same discipline as the db lane: every
// failure lands in status.mesh and the relay path carries on untouched.
// ---------------------------------------------------------------------------

const MESH_SIGNAL_PATH = "/v1/browser/signals";
const MESH_MODULE_URL = new URL("./browser-node/peer-mesh.js", import.meta.url);
const MESH_AGENT_BROKER_TIMEOUT_MS = 6_000;

let meshLane = null;
// shape: { mesh (PeerMesh), brokerPort, brokerRequestId, requestSeq, pendingBroker }

function updateMeshStatus() {
  const summary = meshLane && meshLane.mesh ? meshLane.mesh.summary() : null;
  if (JSON.stringify(status.mesh ?? null) === JSON.stringify(summary)) return;
  setStatus({ mesh: summary });
}

// A mesh failure with no lane yet (module missing, wasm too old to sign):
// surfaced honestly, never hidden, never fatal to anything else.
function noteMeshError(error) {
  const message = String((error && error.message) || error).slice(0, 300);
  if (meshLane && meshLane.mesh) {
    meshLane.mesh.error = message;
    updateMeshStatus();
    return;
  }
  setStatus({
    // Same key set PeerMesh.summary() emits — a consumer must never have to
    // branch on whether the lane got far enough to exist.
    mesh: { protocol: 1, state: "unavailable", known: 0, connected: 0, peers: [], direct: 0, artifactHits: 0, crrRounds: 0, bytesIn: 0, bytesOut: 0, error: message },
  });
}

// One signalling round trip. Proof-of-possession is the SAME shape the
// admission POST uses (signAdmission over "{endpoint_id}:{challenge_ms}"), so
// the fleet arm verifies it with the code path it already has.
async function postMeshSignals(body) {
  if (!node || !session || !status.endpointId) throw new Error("browser node is not running");
  const challengeMs = Date.now();
  return await callApi("POST", MESH_SIGNAL_PATH, session.team, {
    endpoint_id: status.endpointId,
    challenge_ms: challengeMs,
    signature: node.signAdmission(String(challengeMs)),
    ...body,
  });
}

// Broker a page-hosted RTCPeerConnection agent — the sqlite-worker broker
// pattern verbatim (RTCPeerConnection is Window-only, so there is no nested
// fallback here at all: with no page willing to host, the lane is dormant).
async function requestMeshAgent(lane) {
  let lastError = null;
  const candidates = [...ports].sort(
    (a, b) => Number(portVisibility.get(b) ?? true) - Number(portVisibility.get(a) ?? true),
  );
  for (const port of candidates) {
    if (meshLane !== lane) throw new Error("mesh lane superseded during agent broker");
    try {
      return await requestBrokeredMeshAgent(lane, port);
    } catch (error) {
      lastError = error;
    }
  }
  throw lastError || new Error("no connected page could host a peer-connection agent");
}

function requestBrokeredMeshAgent(lane, port) {
  return new Promise((resolve, reject) => {
    const requestId = ++lane.requestSeq;
    const timer = setTimeout(() => {
      if (lane.pendingBroker && lane.pendingBroker.requestId === requestId) lane.pendingBroker = null;
      reject(new Error("page did not host a peer-connection agent in time"));
    }, MESH_AGENT_BROKER_TIMEOUT_MS);
    lane.pendingBroker = {
      requestId,
      resolve: (endpoint) => { clearTimeout(timer); resolve(endpoint); },
      reject: (error) => { clearTimeout(timer); reject(error); },
    };
    try {
      port.postMessage({ type: "meshPeerRequest", requestId });
    } catch (error) {
      clearTimeout(timer);
      if (lane.pendingBroker && lane.pendingBroker.requestId === requestId) lane.pendingBroker = null;
      reject(error);
    }
  });
}

function onMeshPeerPort(port, msg, transferred) {
  const lane = meshLane;
  const endpoint = transferred && transferred[0];
  const pending = lane && lane.pendingBroker;
  if (!lane || !pending || msg.requestId !== pending.requestId) {
    try {
      if (endpoint) endpoint.close();
    } catch {
      /* ignore */
    }
    return;
  }
  lane.pendingBroker = null;
  if (!endpoint) {
    pending.reject(new Error("meshPeerPort arrived without a transferred port"));
    return;
  }
  lane.brokerPort = port;
  lane.brokerRequestId = msg.requestId;
  pending.resolve(endpoint);
}

// The page hosting the agent went away: its PeerConnections died with it. Drop
// the agent so the next handshake re-brokers from a surviving page; the peer
// set, the capability and every fleet path are untouched.
function noteMeshPortRemoved(port) {
  const lane = meshLane;
  if (!lane) return;
  if (lane.pendingBroker) {
    try {
      lane.pendingBroker.reject(new Error("brokering page disconnected"));
    } catch {
      /* already settled */
    }
    lane.pendingBroker = null;
  }
  if (lane.brokerPort !== port) return;
  lane.brokerPort = null;
  if (lane.mesh) lane.mesh.agentLost("brokering page disconnected");
  updateMeshStatus();
}

// ---- what a peer may pull FROM this node ---------------------------------

// Content-addressed only, and only a digest THIS node itself pinned under its
// own server-derived capability. peer-mesh.js has already refused any digest
// outside the peer's authorized set; this is the second, local half of that
// check — a peer can never make this node fetch something on its behalf.
async function serveMeshArtifact(_peerId, digest) {
  if (!fnRuntime || fnRuntime.closed || !fnRuntime.has(digest)) return null;
  const bytes = await readCachedArtifact({ policyDigest: digest });
  return bytes && bytes.length ? bytes : null;
}

// One inbound CRR round from a browser peer. The effective access is the
// MINIMUM of this node's own server-derived grant and the peer's: a
// Public-scope (read_only) peer can never push a row into a tenant replica,
// and a read_only local grant applies nothing from anyone. The responder
// itself re-checks that the request names THIS replica file (a peer of another
// project cannot address ours), and the fleet remains the system of record —
// every batch that lands here still converges through the fleet rounds this
// lane never replaces.
async function serveMeshCrr(_peerId, requestBytes, peerAccess) {
  const lane = dbLane;
  if (!lane || !lane.client || !lane.makeCrrSyncResponder) throw new Error("no live db lane");
  if (lane.sealed) throw new Error("the database grant is sealed");
  const access = lane.capability.access === "read_write" && peerAccess === "read_write" ? "read_write" : "read_only";
  const responder = lane.makeCrrSyncResponder(lane.client, { ...lane.capability, access });
  return await responder(requestBytes, null);
}

async function fetchArtifactFromMesh(descriptor) {
  if (!meshLane || !meshLane.mesh) return null;
  try {
    return await meshLane.mesh.fetchArtifact(descriptor.policyDigest, descriptor.sourceBytes);
  } catch {
    return null;
  }
}

// Direct peers to include in a db sync pass, as sync-client peer entries.
// Empty whenever the lane is dormant, unauthorized, or simply not connected —
// which is why runDbSync can append them unconditionally.
function meshDbPeers() {
  if (!meshLane || !meshLane.mesh || !dbLane) return [];
  try {
    return meshLane.mesh.dbPeers(dbLane.capability.project);
  } catch {
    return [];
  }
}

async function startMeshLane(rawMesh, myEpoch) {
  const owner = node;
  if (!owner || !status.endpointId) return;
  let module;
  let wasm;
  try {
    // Dynamic import for the same reason the sqlite set uses one: a deployed
    // asset set that predates this lane must degrade it honestly, never break
    // the run-node worker.
    module = await import(MESH_MODULE_URL.href);
    wasm = await loadBrowserModule();
  } catch (error) {
    noteMeshError(`peer mesh unavailable: ${String((error && error.message) || error)}`);
    return;
  }
  if (myEpoch !== epoch || node !== owner) return;
  // Feature-detect the identity-binding exports rather than trusting a version
  // constant: the wasm bundle is a build artifact that can legitimately lag the
  // JS by a deploy, and without BOTH halves of the signature there is no way to
  // bind a DTLS fingerprint to an EndpointId — in which case the honest answer
  // is no peer lane at all, never an unauthenticated one.
  if (typeof owner.signMeshEnvelope !== "function" || typeof wasm.verifyMeshEnvelope !== "function") {
    noteMeshError("this browser-node bundle predates peer-mesh signing — peer links stay on the relay path");
    return;
  }
  const capability = module.normalizeMeshCapability(rawMesh);
  if (!capability) return;
  const lane = { mesh: null, brokerPort: null, brokerRequestId: 0, requestSeq: 0, pendingBroker: null };
  meshLane = lane;
  lane.mesh = new module.PeerMesh({
    selfId: status.endpointId,
    sign: (context) => owner.signMeshEnvelope(context),
    verify: (peerId, context, signature) => {
      try {
        return wasm.verifyMeshEnvelope(peerId, context, signature);
      } catch {
        return false; // malformed input from a peer is a failed verification
      }
    },
    postSignals: (body) => postMeshSignals(body),
    requestAgent: () => requestMeshAgent(lane),
    handlers: { artifact: serveMeshArtifact, crr: serveMeshCrr },
    onChange: () => {
      if (meshLane === lane) updateMeshStatus();
    },
  });
  lane.mesh.setCapability(capability);
  updateMeshStatus();
}

async function stopMeshLane() {
  const lane = meshLane;
  if (!lane) return;
  meshLane = null;
  if (lane.brokerPort) {
    // Tell the page to stop its agent NOW — an orphan holds live
    // PeerConnections after the admission that authorized them is gone.
    try {
      lane.brokerPort.postMessage({ type: "meshPeerDone", requestId: lane.brokerRequestId });
    } catch {
      /* page already gone */
    }
  }
  if (lane.mesh) {
    try {
      await lane.mesh.stop();
    } catch {
      /* best effort */
    }
  }
  setStatus({ mesh: null });
}

// One reconcile per successful admission/renewal, alongside the db lane's.
// A lost `mesh` block tears the lane down: peers are a live server-derived
// grant, never a cached one.
async function reconcileMeshLane(capability, myEpoch) {
  const raw = capability && capability.mesh;
  if (raw === undefined || raw === null) {
    await stopMeshLane();
    return;
  }
  if (!meshLane) {
    await startMeshLane(raw, myEpoch);
    return;
  }
  const module = await import(MESH_MODULE_URL.href);
  if (myEpoch !== epoch || !meshLane) return;
  const next = module.normalizeMeshCapability(raw);
  // `enabled: false` (or a set with no peers left) is a real withdrawal, not a
  // pause: tear the lane down rather than leave it idling on a grant the
  // server has stopped issuing.
  if (!next || !next.peers.length) {
    await stopMeshLane();
    return;
  }
  meshLane.mesh.setCapability(next);
  updateMeshStatus();
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
    // The db lane rides the same renewal spine: start on a fresh grant,
    // adopt the renewed capability, wipe on a lost block. It NEVER fails the
    // admission — a db-lane failure lands in status.db and retries on its own
    // schedule.
    try {
      await reconcileDbLane(admitResult && admitResult.capability, myEpoch);
    } catch (dbError) {
      noteDbLaneError(dbError);
    }
    if (myEpoch !== epoch) return "stale";
    // The peer-mesh lane rides the same spine, after the db lane (its CRR
    // handler reads dbLane) and under the same rule: it never fails an
    // admission. A fleet with no signalling arm, a page that cannot host an
    // agent, and a peer that never connects are all just "no direct link".
    try {
      await reconcileMeshLane(admitResult && admitResult.capability, myEpoch);
    } catch (meshError) {
      noteMeshError(meshError);
    }
    if (myEpoch !== epoch) return "stale";
    admittedAddrJson = attemptedAddr;
    renewBackoffMs = RENEW_BACKOFF_BASE_MS;
    const lifecycle = anyVisible() ? "online" : "suspended";
    setStatus({ admission: "granted", lifecycle, protocolMismatch: "none", sessionStale: false, lastError: null });
    nextDelay = RENEW_INTERVAL_MS;
    outcome = "success";
  } catch (error) {
    if (myEpoch !== epoch) return "stale";
    const preUpgrade = isPreUpgradeTargetlessRefusal(error);
    const message = preUpgrade
      ? "This node hasn't rolled forward to support running without a serve target yet — retrying automatically, no action needed."
      : String((error && error.message) || error);
    const mismatch = classifyAdmissionError(error);
    const sessionStale = isSessionStaleError(error);
    const terminal = isTerminalAdmissionError(error) && !preUpgrade;
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
      // Terminal denial (e.g. deployment_not_ready / forbidden) is treated as
      // revocation (contract §5): wipe the replica, no final push — the grant
      // is already gone server-side.
      await stopDbLane({ wipe: true, finalPush: false, owner: doomed });
      // Peer links are a grant too: a denied admission means the peer set is
      // no longer authorized, so every direct channel closes with it.
      await stopMeshLane();
      await discardStaleAttempt(doomed, endpointId, renewalTeam);
      outcome = "terminal";
    } else {
      // Non-terminal: the lease may lapse while renewals fail — the replica
      // is SEALED (no exchange) until a renewal re-grants, never wiped.
      sealDbLane();
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
  // Defensive: no db lane may outlive its epoch (stop() tears it down; this
  // fences a future path that forgets — two lanes would fight over the
  // origin's single OPFS access-handle pool). Never wipes here: an orphaned
  // lane's replica is the admission lifecycle's call, not this fence's.
  if (dbLane) await stopDbLane({ wipe: false, finalPush: false });
  // Same fence for the peer lane: a channel authorized by a previous epoch's
  // admission must never survive into a new one.
  if (meshLane) await stopMeshLane();
  currentAddrJson = null;
  admittedAddrJson = null;
  renewKickPending = false;
  renewBackoffMs = RENEW_BACKOFF_BASE_MS;
  // The serve target is OPTIONAL (browser-node-optional-serve-target) —
  // normalized to "" here so every downstream check is one shape, and a start
  // message that simply omits it is a first-class "run a node, serve nothing
  // yet" request rather than a malformed one.
  // browser-auto-serve-eligible-set: naming a deployment is a deliberate PIN
  // and always wins; otherwise the page's choice decides between the automatic
  // set and capacity-only. An absent serveMode from a message this build didn't
  // write (an owner-handoff replay persisted before the field existed) resolves
  // to "auto", which is this build's default — never to a target the user
  // never picked, since auto is derived entirely server-side from their own
  // tenant's Ready deployments.
  const serveMode = msg.deployment ? "pinned" : msg.serveMode === "none" ? "none" : "auto";
  session = {
    deployment: msg.deployment || "",
    fn: msg.fn || "",
    digest: msg.digest || "",
    serveMode,
    scope: msg.scope,
    team: msg.team,
    relay: msg.relay,
  };
  // endpointId is cleared here, not just set on success: it survived a failed
  // or stopped run otherwise, and the presence heartbeat is keyed on it — so a
  // restart would keep republishing a satellite for an endpoint that no longer
  // exists until the record's TTL happened to catch up.
  setStatus({
    lifecycle: "starting",
    lastError: null,
    admission: "pending",
    sessionStale: false,
    endpointId: null,
    serveMode,
  });
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
    // Identity persistence (bn-impl-key-persistence): `loadOrCreateSeed`'s
    // candidate is only ever READ when no seed is persisted yet (see
    // identity.js's `existing` branches — an existing record never touches
    // it), so it needs no network boot to produce: a raw 32 bytes from
    // WebCrypto's CSPRNG is exactly what an ed25519 seed is, identical in
    // distribution to what `BrowserNode.boot()`'s internal `SecretKey`
    // generation would have produced. Previously this called
    // `BrowserNode.boot(...)` TWICE in series on a cold start with no
    // persisted seed — a full throwaway network boot (bind + the
    // `RELAY_ONLINE_TIMEOUT` wait inside it) purely to read
    // `scratch.secretHex()`, discarded unread on every RETURNING user's boot
    // (`existing` present) and, worse, on a first-ever boot, DOUBLING both
    // the relay-online wait and its failure probability before the node the
    // user actually gets ever starts connecting — exactly the shape of a
    // "browser relay online deadline exceeded" report on a cold start.
    // `persistence` (bn-safari-sharedworker-cryptokey-dataclone) reports WHICH
    // custody mode holds the identity — "memory" means a worker restart on
    // this engine forfeits it; surfaced in status below, never hidden.
    const freshSeedHex = Array.from(crypto.getRandomValues(new Uint8Array(32)))
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    const { seedHex, persistence } = await loadOrCreateSeed(freshSeedHex);
    if (myEpoch !== epoch) return; // stop() ran during identity load
    const n = await BrowserNode.boot(orderedRelay, msg.discovery || null, seedHex);
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
    // The db replica seals while offline (no exchange possible); the renewal
    // on network restore unseals it.
    sealDbLane();
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
    notePortRemoved(port); // the db lane re-brokers if this page held its sqlite worker
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
    // The db lane's local revoke (contract §5): best-effort final push while
    // the grant is still live, then wipe the OPFS replica — all BEFORE the
    // admission DELETE (the fleet re-checks the grant on every round) and
    // before the node closes (the push needs the transport).
    await stopDbLane({ wipe: true, finalPush: true });
    // Then the peer links: after the final push (which may legitimately ride a
    // direct channel) and before the admission DELETE, so the `bye` envelopes
    // still have a live signalling grant to travel on.
    await stopMeshLane();
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
  // A lane without a node shouldn't exist (the lane dies with its epoch) —
  // fence it anyway, without a wipe: stop()'s wipe path above is the one that
  // had transport + a live grant for the final push.
  if (dbLane) await stopDbLane({ wipe: false, finalPush: false });
  if (meshLane) await stopMeshLane();
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
    // Cleared with the rest of the run (browser-node-optional-serve-target):
    // `serving` describes THIS run's pinned artifacts, so leaving it true
    // across a stop would carry a claim about a node that no longer exists
    // into whatever starts next.
    serving: false,
    serveMode: "none",
    db: null,
    mesh: null,
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
    // The db lane's page-broker handshake + control ops
    // (bn-run-node-db-sync-wiring; see the file header for the contract).
    else if (msg.type === "dbWorkerPort") onDbWorkerPort(port, msg, e.ports);
    // The peer-mesh lane's page-agent handshake (bn-browser-peer-webrtc-mesh;
    // RTCPeerConnection is Window-only, see the file header).
    else if (msg.type === "meshPeerPort") onMeshPeerPort(port, msg, e.ports);
    else if (msg.type === "dbExec") handleDbExec(port, msg, true);
    else if (msg.type === "dbQuery") handleDbExec(port, msg, false);
    else if (msg.type === "dbSyncNow") handleDbSyncNow(port, msg);
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
  // A fresh page connection is a fresh potential broker: a db lane stuck in
  // "error" because no page could host its sqlite worker retries promptly
  // instead of waiting out the respawn backoff. ("opening" needs no kick —
  // the broker handshake has its own timeout into the error path.)
  if (dbLane && dbLane.state === "error" && !dbLane.nested && !dbLane.endpoint) {
    scheduleDbRespawn(dbLane, 250);
  }
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
