"use client";

// The typed status contract shared by the SharedWorker (public/run-node-worker.js, plain JS: this file documents its wire
// shape), the navbar control and the /run-node page; the worker's generation counter is the single source of truth for
// "is this update newer than the one I rendered".

export type NodeLifecycle = "stopped" | "starting" | "online" | "degraded" | "suspended" | "error";
export type AdmissionState = "none" | "pending" | "granted" | "denied" | "revoked" | "expired";
export type GeoConsent = "undecided" | "granted" | "denied";
/** "outdated" needs a forced reload (no retry can succeed once the server's floor passed this build); "server_upgrading" is
 *  the normal mid-rollout shape and resolves on retry, never a reload prompt. */
export type ProtocolMismatch = "none" | "outdated" | "server_upgrading";

/** Wire ABI of the page <-> SharedWorker message contract: bump when the message SHAPE changes in a way an older worker or
 *  page cannot interpret, and keep equal to `HOST_ABI_VERSION` in public/run-node-worker.js. `use-run-node.ts` embeds it in the
 *  worker URL and SharedWorker name, which is what forces a fresh worker (a live one outlives page reloads). */
export const HOST_ABI_VERSION = 4;

/** Whether the worker's global is cross-origin isolated (the synchronous `receiveMessageOnPort` half of node-worker exists only
 *  then). `syncBridge: false` is an expected value, not an error (Safari, headerless pages, a service worker that drops the
 *  headers): async-only still runs mesh, relay, presence, database replication and the function lane. */
export type NodeIsolation = {
  crossOriginIsolated: boolean;
  sharedArrayBuffer: boolean;
  syncBridge: boolean;
};

export interface RunNodeStatus {
  /** Monotonic generation: a status with a lower version than one already applied is stale and dropped, never merged. */
  version: number;
  lifecycle: NodeLifecycle;
  endpointId: string | null;
  relay: string | null;
  admission: AdmissionState;
  geoConsent: GeoConsent;
  protocolMismatch: ProtocolMismatch;
  /** The connected SharedWorker's abiVersion is older than HOST_ABI_VERSION (or absent = pre-versioning). A page reload does NOT
   *  fix it (the worker persists): every tab must close. Distinct from protocolMismatch (this browser vs the FLEET). */
  hostAbiStale: boolean;
  /** The last admission/renewal failed because THIS TAB's platform session is stale (not identity or wire protocol); self-clears
   *  on the next success and is never terminal. */
  sessionStale: boolean;
  /** coop-coep-fleet-wide (additive wire key): null until the worker reports,
   *  i.e. before the SharedWorker has connected — never assumed true. */
  isolation: NodeIsolation | null;
  lastError: string | null;
  updatedMs: number;
  /** Distinct tabs attached to the owning worker; defaults to 1 for a pre-tabCount worker (absent on the wire). */
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

/** Merges an incoming status only when strictly newer: the stale-write fencing of browser_admission.rs's version field,
 *  guarding against a delayed worker message clobbering fresher UI state. */
export function applyStatus(current: RunNodeStatus, incoming: RunNodeStatus): RunNodeStatus {
  if (incoming.version <= current.version) return current;
  return incoming;
}

/** The worker's ADDITIVE db-lane status key, present only while the admitted project's capability carries a server-derived `db`
 *  block; consumers widen `RunNodeStatus` with `{ db?: DbLaneStatus | null }` (shared by /run-node and the Storages panel). */
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

/** The worker's ADDITIVE function-lane status key (consumers widen `RunNodeStatus` with `{ functions?: FunctionLaneStatus | null }`):
 *  `serving[]` names what is actually PINNED, never what was requested; `failed[]` names artifacts the next renewal retries. */
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

/** The worker's ADDITIVE synchronous-`fs` status key (consumers widen `RunNodeStatus` with `{ syncFs?: SyncFsLaneStatus | null }`):
 *  `ready` means node-worker's service worker covers dist/worker.js, which makes the guest's BLOCKING `fs` requests answerable;
 *  `unavailable` carries a named `sync_fs_<reason>` message that states the fix. */
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

/** Which serve lane the running node ASKED for (the worker echoes its session): the automatic eligible set, one pinned function,
 *  or capacity only; distinct from `serving`, which is what is pinned. */
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

/** One sentence a user can act on (or dismiss); no isolation at all is a server/header/browser question, isolation without
 *  `SharedArrayBuffer` is a browser-policy question. */
export function isolationLabel(isolation: NodeIsolation): string {
  if (isolation.syncBridge) return "Cross-origin isolated — synchronous bridge available";
  if (!isolation.crossOriginIsolated) {
    return "Async-only mode: this page is not cross-origin isolated, so the synchronous bridge is unavailable (this browser, or the headers on this page). Everything else — mesh, database sync and serving — runs normally.";
  }
  return "Async-only mode: this browser reports isolation but withholds SharedArrayBuffer, so the synchronous bridge is unavailable. Everything else runs normally.";
}
