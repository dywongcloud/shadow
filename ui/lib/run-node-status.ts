"use client";

// One typed status contract shared by the SharedWorker (public/run-node-worker.js,
// plain JS — this file documents its wire shape), the navbar control, and the
// /run-node page. Keeping every consumer on ONE shape means the worker's
// generation counter is the single source of truth for "is this update newer
// than the one I already rendered" — no consumer invents its own booleans.

export type NodeLifecycle = "stopped" | "starting" | "online" | "degraded" | "suspended" | "error";
export type AdmissionState = "none" | "pending" | "granted" | "denied" | "revoked" | "expired";
export type GeoConsent = "undecided" | "granted" | "denied";
/** bn-p2p-version-negotiation: the two protocol-mismatch directions need
 *  distinct UI treatment. "outdated" needs a forced reload (no retry will
 *  ever succeed against a server that has moved its floor past this build);
 *  "server_upgrading" is the normal mid-rollout shape (this node hasn't
 *  caught up yet) and resolves itself on retry, never a reload prompt. */
export type ProtocolMismatch = "none" | "outdated" | "server_upgrading";

/** bn-p2p-version-negotiation (remaining scope, item 2/3: the host-operation
 *  ABI): the {type:"...",...} postMessage contract between this page and the
 *  SharedWorker had no version marker at all, so a stale-but-still-running
 *  worker (a SharedWorker outlives any single tab's reload -- it only
 *  restarts once every connecting tab has closed) paired with fresh page JS
 *  had no way to detect the mismatch. Bump this whenever the message SHAPE
 *  changes in a way an older worker/page can't safely interpret; keep the
 *  literal number in sync with public/run-node-worker.js's own copy (same
 *  cross-language-constant pattern as PROTOCOL_VERSION there, which mirrors
 *  hive_browser_proto::BROWSER_PROTOCOL_VERSION -- a plain JS file served
 *  from public/ can't import a TS module, so this can't be a single shared
 *  export). */
/* v2 (2026-08-05): the worker's boot/admission behaviour changed materially
 * (non-fatal relay-online wait + wasm v15, session re-mint, endpointId reset).
 * Bumping is not cosmetic — a live SharedWorker survives page reloads, so
 * without a bump every already-open tab kept serving the PREVIOUS worker
 * build and users kept hitting an error string that no longer exists in the
 * deployed wasm. `use-run-node.ts` now also embeds this number in the worker
 * URL and SharedWorker name, which is what actually forces a fresh worker;
 * this constant remains the detector of last resort. */
/* v3 (2026-08-05, browser-node-optional-serve-target): the `start` message's
 * deployment/fn/digest are now OPTIONAL and empty means "attach to nothing".
 * A pre-v3 worker receiving that message admits with an empty target and then
 * throws "capability carries a malformed digest" on the serve-less capability
 * it gets back (it has no `serving: false` branch), wedging in a permanent
 * retry loop. Bumping re-keys the worker URL + SharedWorker name so a reloaded
 * page attaches to a worker that understands the shape, per the mechanism
 * described for v2 above. */
/* v4 (2026-08-05, browser-auto-serve-eligible-set): the `start` message carries
 * `serveMode` ("auto" | "none"), and the admission capability answers with an
 * ARRAY of authorized artifacts (`artifacts[]`) rather than one hand-picked
 * descriptor. A pre-v4 worker reads only the flat compat mirror the server
 * still emits, so it pins the FIRST artifact while the fleet routes every one
 * of them to it — the extra routes hit its not-pinned-locally rejection and
 * open their per-digest circuit. Bumping re-keys the worker URL + SharedWorker
 * name so a reloaded page attaches to a worker that pins the whole set. */
export const HOST_ABI_VERSION = 4;

/** coop-coep-fleet-wide: whether this worker's global is cross-origin
 *  isolated, i.e. whether the SYNCHRONOUS half of node-worker
 *  (`receiveMessageOnPort`) exists at all. Additive on the wire, like `db`
 *  below — the SharedWorker publishes it, the page renders it.
 *
 *  `syncBridge: false` is an honest, EXPECTED value, not an error: Safari
 *  implements neither `COEP: credentialless` nor the COOP/COEP pair this lane
 *  serves, a page served without the headers is never isolated, and a service
 *  worker that drops them un-isolates it. Async-only means mesh, relay
 *  identity, presence, database replication and the function lane all still
 *  run — only a synchronous port drain does not. */
export type NodeIsolation = {
  crossOriginIsolated: boolean;
  sharedArrayBuffer: boolean;
  syncBridge: boolean;
};

export interface RunNodeStatus {
  /** Monotonic generation — a status message with a lower version than one
   *  already applied is stale (e.g. from a delayed duplicate) and must be
   *  dropped, never merged over newer state. */
  version: number;
  lifecycle: NodeLifecycle;
  endpointId: string | null;
  relay: string | null;
  admission: AdmissionState;
  geoConsent: GeoConsent;
  protocolMismatch: ProtocolMismatch;
  /** True when the connected SharedWorker's own reported abiVersion is older
   *  than HOST_ABI_VERSION (or absent entirely, meaning a pre-versioning
   *  worker) -- distinct from protocolMismatch, which is about this browser's
   *  wire compatibility with the FLEET, not this tab's compatibility with its
   *  own background worker. A plain page reload does NOT fix this (the
   *  SharedWorker instance persists across it); every tab must close first. */
  hostAbiStale: boolean;
  /** bn-p2p-reconnect-state (auth-renewal input): true while the last
   *  admission/renewal attempt failed specifically because THIS TAB's
   *  platform session is stale, not because of the node's own identity or
   *  wire protocol. Self-clears on the next successful renewal; never
   *  terminal — a background session-cookie refresh (e.g. Clerk) can make
   *  the very next retry succeed with no page reload needed, unlike
   *  protocolMismatch === "outdated". */
  sessionStale: boolean;
  /** coop-coep-fleet-wide (additive wire key): null until the worker reports,
   *  i.e. before the SharedWorker has connected — never assumed true. */
  isolation: NodeIsolation | null;
  lastError: string | null;
  updatedMs: number;
  /** Multi-tab dedup UI (bn-ui-sharedworker-owner): distinct tabs currently
   *  attached to the owning worker, real vs. assumed-just-this-one. Defaults
   *  to 1 for a pre-tabCount worker instance (absent on the wire) so an old
   *  running worker never renders as "0 tabs". */
  tabCount: number;
}

export function initialRunNodeStatus(): RunNodeStatus {
  return {
    version: 0,
    lifecycle: "stopped",
    endpointId: null,
    relay: null,
    admission: "none",
    geoConsent: "undecided",
    protocolMismatch: "none",
    hostAbiStale: false,
    sessionStale: false,
    isolation: null,
    lastError: null,
    updatedMs: 0,
    tabCount: 1,
  };
}

/** Merge an incoming status message, rejecting anything not strictly newer —
 *  the same stale-write-fencing shape every other replicated store in this
 *  codebase uses (see browser_admission.rs's version field), applied here to
 *  guard against a delayed worker message clobbering fresher UI state. */
export function applyStatus(current: RunNodeStatus, incoming: RunNodeStatus): RunNodeStatus {
  if (incoming.version <= current.version) return current;
  return incoming;
}

// bn-run-node-db-sync-wiring / bn-storages-page-browser-db-wiring: the
// worker's additive db-lane status field. Present only while the admitted
// project's capability carries a server-derived `db` block (a fluid.json
// top-level `browser_db` opt-in, or the Storages page's dashboard-managed
// equivalent — see `crates/hive-cloud/src/browser_db.rs`). Hoisted HERE
// (rather than staying a page-local widened type on /run-node) so a second
// consumer — the Storages page's live-status panel — can read the exact same
// wire shape instead of re-declaring it; both pages widen `RunNodeStatus`
// with `{ db?: DbLaneStatus | null }` since the field isn't in the core
// status contract's own TS type (the worker still sends it as an ADDITIVE key).
export type DbLaneStatus = {
  project: string | null;
  dbFile: string | null;
  access: "read_write" | "read_only" | null;
  state: "opening" | "idle" | "syncing" | "sealed" | "error";
  persisted: boolean | "unknown" | null;
  peers: number;
  lastSyncMs: number | null;
  sites: number;
  siteVersion: number;
  error: string | null;
};

/** browser-auto-serve-eligible-set: the worker's additive function-lane status.
 *  Like `db` above this is an ADDITIVE wire key (consumers widen
 *  `RunNodeStatus` with `{ functions?: FunctionLaneStatus | null }`), and like
 *  `serving` it describes what is actually PINNED — never what was requested.
 *  `serving[]` names the pinned digests' deployments/functions so the dashboard
 *  can say what this node carries; `failed[]` names artifacts the last
 *  reconcile could not pin, which the next renewal retries. */
export type FunctionLaneStatus = {
  /** Policy digests currently pinned in the worker runtime. */
  pinned: string[];
  /** Live invoker grants across every pinned digest. */
  grants: number;
  /** Invocations served since this run started. */
  served: number;
  serving: { digest: string; deployment?: string; function?: string; project?: string }[];
  failed: { digest: string; error: string }[];
};

/** bn-node-worker-service-worker-sync-fs: the worker's additive synchronous-
 *  `fs` status field. Like `db` above this is an ADDITIVE wire key (consumers
 *  widen `RunNodeStatus` with `{ syncFs?: SyncFsLaneStatus | null }`).
 *
 *  `state: "ready"` means node-worker's service worker is registered at a
 *  scope that covers dist/worker.js — which is what makes the guest's
 *  BLOCKING synchronous `fs` requests answerable at all. Without it there is
 *  no working `require`, so this is a prerequisite of the node-worker
 *  substrate, not an optimisation. `unavailable` carries a named
 *  `sync_fs_<reason>` message that also states the fix. */
export type SyncFsLaneStatus = {
  state: "registering" | "ready" | "unavailable";
  /** Registration scope, e.g. `https://host/browser-node/node-worker/`. */
  scope: string | null;
  /** The service worker script actually registered (`dist/sw.js`). */
  scriptURL: string | null;
  /** Scope-relative prefix the node worker POSTs its frames to. */
  prefix: string | null;
  error: string | null;
};

export function syncFsStateLabel(syncFs: SyncFsLaneStatus): string {
  switch (syncFs.state) {
    case "registering":
      return "registering service worker…";
    case "ready":
      return "ready";
    case "unavailable":
      return "unavailable";
  }
}

/** Which serve lane the running node ASKED for (the worker echoes its own
 *  session): the automatic eligible set, one deliberately pinned function, or
 *  capacity only. Distinct from `serving`, which is about what is pinned. */
export type ServeMode = "auto" | "pinned" | "none";

export function dbLaneStateLabel(db: DbLaneStatus): string {
  switch (db.state) {
    case "opening":
      return "opening local replica…";
    case "syncing":
      return "syncing…";
    case "sealed":
      return "sealed — resumes on re-admission";
    case "error":
      return "error";
    case "idle":
      return db.lastSyncMs ? `last synced ${Math.max(0, Math.round((Date.now() - db.lastSyncMs) / 1000))}s ago` : "idle";
  }
}

export function lifecycleLabel(state: NodeLifecycle): string {
  switch (state) {
    case "stopped": return "Stopped";
    case "starting": return "Starting…";
    case "online": return "Online";
    case "degraded": return "Degraded";
    case "suspended": return "Suspended";
    case "error": return "Error";
  }
}

/** coop-coep-fleet-wide: one sentence a user can act on (or dismiss). The two
 *  false-ish shapes are named separately because their remedies differ — no
 *  isolation at all is a server/header/browser question, isolation without
 *  `SharedArrayBuffer` is a browser policy question. */
export function isolationLabel(isolation: NodeIsolation): string {
  if (isolation.syncBridge) return "Cross-origin isolated — synchronous bridge available";
  if (!isolation.crossOriginIsolated) {
    return "Async-only mode: this page is not cross-origin isolated, so the synchronous bridge is unavailable (this browser, or the headers on this page). Everything else — mesh, database sync and serving — runs normally.";
  }
  return "Async-only mode: this browser reports isolation but withholds SharedArrayBuffer, so the synchronous bridge is unavailable. Everything else runs normally.";
}
