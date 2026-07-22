"use client";

/**
 * Web Push client helpers — shared by the notification bell's "Enable push"
 * banner (`components/notifications.tsx`) and the delivery-management section
 * on `/settings/notifications`. All server calls ride the same-origin `/cloud`
 * proxy via `apiGet`/`apiSend`, so the auth cookie + `x-hive-team` tenant
 * header are attached exactly like every other dashboard request.
 */

import { apiGet, apiSend } from "@/lib/api";

// ---- API contract types (mirror the backend push routes) ----

export interface PushDevice {
  endpoint: string;
  label: string;
  created_ms: number;
}
export interface SmsConfig {
  phone: string;
  enabled: boolean;
  /** A number only receives notifications once its owner confirms the code we
   *  text it — so notifications can never be pointed at an unowned number. */
  verified: boolean;
}
/** Response of `PUT /v1/push/sms`: setting a NEW/changed number texts a code
 *  and needs verification (`code_sent`); toggling an already-verified number
 *  returns `verified` immediately. */
export interface SmsPutResult {
  ok: boolean;
  verified: boolean;
  code_sent: boolean;
  note?: string;
}
export interface PushSettings {
  devices: PushDevice[];
  sms: SmsConfig | null;
  sms_quota: number | null;
}
export interface PushTestResult {
  web_push: { sent: number; failed: number; errors: string[]; devices?: number; pruned?: number };
  sms: { attempted: boolean; ok: boolean; error?: string };
}

/** Decode a base64url VAPID public key into the raw bytes
 *  `PushManager.subscribe` wants as `applicationServerKey`. */
export function urlBase64ToUint8Array(base64String: string): Uint8Array {
  const padding = "=".repeat((4 - (base64String.length % 4)) % 4);
  const base64 = (base64String + padding).replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(base64);
  const out = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

/** Encode a `PushSubscription.getKey(...)` ArrayBuffer as base64url (the wire
 *  format the backend stores for `p256dh`/`auth`). Empty string for null. */
export function bufferToBase64url(buf: ArrayBuffer | null): string {
  if (!buf) return "";
  const bytes = new Uint8Array(buf);
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/g, "");
}

/** True when this browser can do Web Push at all (SW + PushManager + Notification). */
export function pushSupported(): boolean {
  return (
    typeof window !== "undefined" &&
    "serviceWorker" in navigator &&
    !!navigator.serviceWorker &&
    "PushManager" in window &&
    "Notification" in window
  );
}

/** Short human label for THIS device, e.g. "Chrome · macOS" — shown in the
 *  registered-devices list so a user can tell their subscriptions apart. */
export function deviceLabel(): string {
  if (typeof navigator === "undefined") return "Unknown device";
  const ua = navigator.userAgent;
  // Order matters: Edge/Opera embed "Chrome", Chrome embeds "Safari".
  const browser = /Edg\//.test(ua)
    ? "Edge"
    : /OPR\//.test(ua)
      ? "Opera"
      : /Firefox\//.test(ua)
        ? "Firefox"
        : /Chrome\//.test(ua)
          ? "Chrome"
          : /Safari\//.test(ua)
            ? "Safari"
            : "Browser";
  // iPadOS 13+ masquerades as macOS; maxTouchPoints tells them apart.
  const os = /iPhone|iPod/.test(ua)
    ? "iOS"
    : /iPad/.test(ua) || (/Mac/.test(ua) && navigator.maxTouchPoints > 1)
      ? "iPadOS"
      : /Android/.test(ua)
        ? "Android"
        : /Mac OS X|Macintosh/.test(ua)
          ? "macOS"
          : /Windows/.test(ua)
            ? "Windows"
            : /Linux/.test(ua)
              ? "Linux"
              : "Unknown OS";
  return `${browser} · ${os}`;
}

/** The browser's current PushSubscription (if any) — used by the settings page
 *  to mark "this device" in the registered list. Never throws, never hangs:
 *  uses `getRegistration()` (resolves undefined when no SW is registered)
 *  instead of `.ready` (which waits forever in that case). */
export async function currentPushSubscription(): Promise<PushSubscription | null> {
  if (!pushSupported()) return null;
  try {
    const reg = await navigator.serviceWorker.getRegistration();
    if (!reg) return null;
    return await reg.pushManager.getSubscription();
  } catch {
    return null;
  }
}

/**
 * Full enable-push flow, shared verbatim by the bell banner and the settings
 * page: request permission → wait for the service worker → reuse or create the
 * browser PushSubscription (VAPID key fetched from the backend) → ALWAYS
 * register it server-side. The unconditional POST is deliberate: registration
 * is an idempotent upsert by endpoint scoped to the CURRENT tenant, so calling
 * this again after a team switch re-homes the same browser subscription under
 * the newly-active tenant instead of silently skipping it.
 *
 * Throws on any failure (including permission not granted) — callers branch on
 * `Notification.permission` to distinguish an explicit user denial from a
 * transient error worth retrying.
 */
export async function subscribePush(): Promise<PushSubscription> {
  if (!pushSupported()) throw new Error("push notifications are not supported in this browser");
  const permission = await window.Notification.requestPermission();
  if (permission !== "granted") throw new Error(`notification permission ${permission}`);

  // `.ready` waits for an ACTIVE registration — normally instant since
  // pwa-register.tsx registers /sw.js on load, but bound it so a registration
  // that never activates (e.g. SW blocked by the browser) surfaces as an error
  // instead of an eternally-pending click handler.
  const reg = await Promise.race([
    navigator.serviceWorker.ready,
    new Promise<never>((_, reject) =>
      setTimeout(() => reject(new Error("service worker never became ready")), 15_000),
    ),
  ]);

  let sub = await reg.pushManager.getSubscription();
  if (!sub) {
    const { key } = await apiGet<{ key: string }>("/v1/push/vapid-key");
    sub = await reg.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: urlBase64ToUint8Array(key),
    });
  }

  await apiSend("POST", "/v1/push/subscribe", {
    endpoint: sub.endpoint,
    p256dh: bufferToBase64url(sub.getKey("p256dh")),
    auth: bufferToBase64url(sub.getKey("auth")),
    label: deviceLabel(),
  });
  return sub;
}
