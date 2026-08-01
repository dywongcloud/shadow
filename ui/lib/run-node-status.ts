"use client";

// One typed status contract shared by the SharedWorker (public/run-node-worker.js,
// plain JS — this file documents its wire shape), the navbar control, and the
// /run-node page. Keeping every consumer on ONE shape means the worker's
// generation counter is the single source of truth for "is this update newer
// than the one I already rendered" — no consumer invents its own booleans.

export type NodeLifecycle = "stopped" | "starting" | "online" | "degraded" | "suspended" | "error";
export type AdmissionState = "none" | "pending" | "granted" | "denied" | "revoked" | "expired";
export type GeoConsent = "undecided" | "granted" | "denied";

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
