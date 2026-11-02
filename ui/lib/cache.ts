"use client";

// Lightweight client cache for global or user-scoped assets (the Composio catalog, your GitHub repos): an in-memory Map backed by sessionStorage (survives
// reloads within the tab). Only for data that is NOT tenant-scoped: tenant-scoped data flows through usePoll, which clears on team switch, and caching it here
// could leak across accounts.

interface Entry<T> {
  t: number;
  v: T;
  /** The TTL this entry was stored under (a provisional answer is stored under a shorter one). Absent = the caller's TTL. */
  ttl?: number;
  /** Provisional: an answer that could not be confirmed (a status whose reachability check never ran). It is kept in
   *  memory only, never in sessionStorage, so a flaky minute cannot pin a tab to a degraded answer for the whole TTL. */
  p?: boolean;
}

const mem = new Map<string, Entry<unknown>>();
const PREFIX = "oe_cache:";

/** A degraded answer: a connection status that holds a credential but could not verify it just now. Cached briefly and
 *  never persisted, so the next poll re-asks and a transient blip does not outlive it. */
const PROVISIONAL_TTL_MS = 15_000;

function isProvisional(v: unknown): boolean {
  const o = v as { degraded?: unknown; reachability?: unknown } | null;
  return !!o && typeof o === "object" && (o.degraded === true || o.reachability === "unavailable" || o.reachability === "unknown");
}

const ttlOf = <T,>(e: Entry<T>, ttlMs: number): number => Math.min(e.ttl ?? ttlMs, ttlMs);

/** `cacheable` decides whether a RESPONSE is worth remembering (default: yes): a "not configured" answer describes server config, not data, and caching it
 *  keeps every session that already asked on the stale answer until its TTL expires, so a caller that can tell a degraded answer from a good one passes it. */
export async function cachedJson<T>(
  url: string,
  ttlMs = 5 * 60_000,
  init?: RequestInit,
  cacheable: (v: T) => boolean = () => true,
): Promise<T> {
  const now = Date.now();

  const m = mem.get(url);
  if (m && now - m.t < ttlOf(m, ttlMs)) return m.v as T;

  if (typeof window !== "undefined") {
    try {
      const raw = sessionStorage.getItem(PREFIX + url);
      if (raw) {
        const e = JSON.parse(raw) as Entry<T>;
        if (!e.p && now - e.t < ttlOf(e, ttlMs)) {
          mem.set(url, e);
          return e.v;
        }
      }
    } catch {}
  }

  const r = await fetch(url, init);
  if (!r.ok) throw new Error(`${url} -> ${r.status}`);
  const v = (await r.json()) as T;
  if (!cacheable(v)) return v;
  const provisional = isProvisional(v);
  const e: Entry<T> = {
    t: now,
    v,
    ...(provisional ? { ttl: Math.min(ttlMs, PROVISIONAL_TTL_MS), p: true } : {}),
  };
  mem.set(url, e);
  if (!provisional && typeof window !== "undefined") {
    try {
      sessionStorage.setItem(PREFIX + url, JSON.stringify(e));
    } catch {}
  }
  return v;
}

/** Drop a cached entry (call after an action that changes it, e.g. connecting). */
export function invalidate(url: string) {
  mem.delete(url);
  if (typeof window !== "undefined") sessionStorage.removeItem(PREFIX + url);
}
