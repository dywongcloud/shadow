"use client";

// Client-side types + API calls for the low-trust browser-node admission and
// presence stores (crates/hive-cloud/src/browser_admission.rs +
// browser_presence.rs). Mirrors the Rust structs field-for-field.

import { apiGet, apiSend } from "./api";

/** Must match `hive_browser_proto::BROWSER_PROTOCOL_VERSION` exactly — there is
 *  no build-time link between this constant and the Rust one, so a bump on
 *  either side without the other fails closed (the backend rejects a mismatched
 *  version rather than silently admitting a skewed peer). */
export const BROWSER_PROTOCOL_VERSION = 0;

export type BrowserScope = "team" | "public";

export interface BrowserAdmission {
  endpoint_id: string;
  addr_json: string;
  deployment: string;
  function: string;
  digest: string;
  tenant: string;
  subject: string;
  issued_ms: number;
  expires_ms: number;
  revision: number;
  scope: BrowserScope;
  protocol_version: number;
}

export interface AdmitRequest {
  endpoint_id: string;
  addr_json: string;
  deployment: string;
  function: string;
  digest: string;
  lease_secs?: number;
  scope?: BrowserScope;
  protocol_version: number;
}

export async function admitBrowser(req: AdmitRequest): Promise<BrowserAdmission> {
  const res = await apiSend<{ admission: BrowserAdmission }>("POST", "/v1/browser/admissions", req);
  return res.admission;
}

export async function listBrowserAdmissions(): Promise<BrowserAdmission[]> {
  const res = await apiGet<{ admissions: BrowserAdmission[] }>("/v1/browser/admissions");
  return res.admissions ?? [];
}

export async function revokeBrowserAdmission(endpointId: string): Promise<void> {
  await apiSend("DELETE", `/v1/browser/admissions/${encodeURIComponent(endpointId)}`);
}

export type PresenceState = "starting" | "online" | "degraded" | "suspended";

export interface BrowserPresence {
  endpoint_id: string;
  tenant: string;
  subject: string;
  display_label: string;
  lat: number | null;
  lon: number | null;
  accuracy_km: number | null;
  located_ms: number | null;
  relay_hint: string;
  state: PresenceState;
  issued_ms: number;
  expires_ms: number;
  revision: number;
}

export interface PresenceRequest {
  endpoint_id: string;
  lat?: number | null;
  lon?: number | null;
  accuracy_km?: number | null;
  relay_hint?: string;
  state: PresenceState;
}

/** Persisted geo consent ("granted" | "denied") and the last quantized fix.
 *
 *  Both live in localStorage rather than React state because the presence
 *  heartbeat that reads them runs in `useRunNode` — mounted app-wide by the
 *  sidebar control — while the geolocation UI that WRITES them only exists on
 *  /run-node. Before this split the heartbeat was a `useEffect` inside the
 *  /run-node page, so its cleanup cleared the interval the moment the user
 *  navigated away: opening /network to look at the constellation was itself
 *  the thing that stopped the node's satellite from being refreshed, and the
 *  90s server-side TTL then expired it off the map. */
export const GEO_CONSENT_KEY = "hive_run_node_geo_consent";
export const GEO_COORDS_KEY = "hive_run_node_geo_coords";

/** Lifecycle → the presence state to publish, or `null` while stopped (no
 *  record should exist at all then). Shared so the page and the heartbeat can
 *  never disagree about what counts as "publishable". */
export function presenceState(lifecycle: string): PresenceState | null {
  if (lifecycle === "starting") return "starting";
  if (lifecycle === "online") return "online";
  if (lifecycle === "degraded" || lifecycle === "error") return "degraded";
  if (lifecycle === "suspended") return "suspended";
  return null; // "stopped" — no presence record while stopped
}

/** The coordinates to publish right now: the persisted fix, but ONLY while
 *  consent is still "granted". Read fresh on every heartbeat so revoking
 *  consent takes effect on the next publish without re-subscribing. */
export function readPresenceGeo(): { lat: number | null; lon: number | null } {
  const none = { lat: null, lon: null };
  if (typeof window === "undefined") return none;
  try {
    if (localStorage.getItem(GEO_CONSENT_KEY) !== "granted") return none;
    const raw = localStorage.getItem(GEO_COORDS_KEY);
    if (!raw) return none;
    const parsed = JSON.parse(raw) as { lat?: unknown; lon?: unknown };
    const lat = typeof parsed.lat === "number" && Number.isFinite(parsed.lat) ? parsed.lat : null;
    const lon = typeof parsed.lon === "number" && Number.isFinite(parsed.lon) ? parsed.lon : null;
    return lat === null || lon === null ? none : { lat, lon };
  } catch {
    return none;
  }
}

export async function upsertPresence(req: PresenceRequest): Promise<BrowserPresence> {
  const res = await apiSend<{ presence: BrowserPresence }>("POST", "/v1/browser/presence", req);
  return res.presence;
}

export async function listPresence(): Promise<BrowserPresence[]> {
  const res = await apiGet<{ presence: BrowserPresence[] }>("/v1/browser/presence");
  return res.presence ?? [];
}

export async function clearPresence(endpointId: string): Promise<void> {
  await apiSend("DELETE", `/v1/browser/presence/${encodeURIComponent(endpointId)}`);
}
