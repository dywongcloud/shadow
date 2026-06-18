"use client";

// Lightweight client cache for global / user-scoped assets (the Composio
// catalog, your GitHub repos) so they aren't re-downloaded on every page load
// or client-side navigation. Two tiers: an in-memory Map (instant within a
// session) backed by sessionStorage (survives reloads within the tab).
//
// IMPORTANT: only use this for data that is NOT tenant-scoped. Tenant-scoped
// data (projects, deployments, databases…) flows through usePoll, which clears
// on team switch — caching it here could leak across accounts.

interface Entry<T> {
  t: number;
  v: T;
}

const mem = new Map<string, Entry<unknown>>();
const PREFIX = "oe_cache:";

export async function cachedJson<T>(url: string, ttlMs = 5 * 60_000, init?: RequestInit): Promise<T> {
  const now = Date.now();

  const m = mem.get(url);
  if (m && now - m.t < ttlMs) return m.v as T;

  if (typeof window !== "undefined") {
    try {
      const raw = sessionStorage.getItem(PREFIX + url);
      if (raw) {
        const e = JSON.parse(raw) as Entry<T>;
        if (now - e.t < ttlMs) {
          mem.set(url, e);
          return e.v;
        }
      }
    } catch {
      /* ignore corrupt cache */
    }
  }

  const r = await fetch(url, init);
  if (!r.ok) throw new Error(`${url} -> ${r.status}`);
  const v = (await r.json()) as T;
  const e: Entry<T> = { t: now, v };
  mem.set(url, e);
  if (typeof window !== "undefined") {
    try {
      sessionStorage.setItem(PREFIX + url, JSON.stringify(e));
    } catch {
      /* quota — best effort */
    }
  }
  return v;
}

/** Drop a cached entry (call after an action that changes it, e.g. connecting). */
export function invalidate(url: string) {
  mem.delete(url);
  if (typeof window !== "undefined") sessionStorage.removeItem(PREFIX + url);
}
