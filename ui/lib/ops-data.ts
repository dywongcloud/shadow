import "server-only";

// Server-side counterpart to lib/api.ts's opsGet -- used by admin ops pages'
// server components to fetch an initial paint of live operational data before
// streaming, avoiding the blank-shell-then-client-fetch round trip. Reads the
// SAME env vars as next.config.mjs's ADMIN_OPS rewrite target, so a server
// fetch here hits exactly what the browser's /ops/* proxy would have hit.
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const ADMIN_OPS = process.env.HIVE_ADMIN_OPS || ADMIN;

/**
 * Fetch ops/admin data directly from the backend, server-side. No auth token
 * is forwarded -- proxy.ts middleware already gates every /admin/* page
 * request on isPlatformOwner before this ever runs, and the client's own
 * opsGet (lib/api.ts) sends no bearer token for these reads either (only an
 * informational x-hive-team header the backend does not trust for tenant
 * derivation). Intentionally uncached (no-store): ops data is live
 * operational state, matching the existing no-store policy at the browser
 * cache-control layer (next.config.mjs headers()) -- never `"use cache"`.
 */
export async function fetchOpsServer<T>(path: string): Promise<T> {
  // Bounded: an unreachable admin host must fail FAST with an honest error,
  // never hang the server component's render indefinitely — previously a bare
  // fetch with no timeout at all (ui-cloud-proxy-admin-fallback).
  const r = await fetch(`${ADMIN_OPS}${path}`, {
    cache: "no-store",
    signal: AbortSignal.timeout(10_000),
  });
  if (!r.ok) throw new Error(`ops GET ${path} -> ${r.status}`);
  return (await r.json()) as T;
}
