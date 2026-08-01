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
