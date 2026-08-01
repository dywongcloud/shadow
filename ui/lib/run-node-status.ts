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
export const HOST_ABI_VERSION = 1;

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
  lastError: string | null;
  updatedMs: number;
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
    lastError: null,
    updatedMs: 0,
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
