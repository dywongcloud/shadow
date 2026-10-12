import "server-only";
import { cookies } from "next/headers";

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
  // Forward the operator's own session cookie. Every ops read is gated by the
  // backend's `require_operator`, which reads the `hive_jwt` cookie (auth.rs);
  // without it this server-side fetch was anonymous and answered 403 on every
  // call (witnessed 2026-10-01: 84 operator-403 lines in 2 h on the leader,
  // all from this loopback caller), so the "first paint with real numbers"
  // never actually happened and every admin page showed a skeleton until the
  // first client poll. The cookie is the same credential the browser sends on
  // `/ops/*`; proxy.ts has already gated the page on the owner allow-list.
  const headers: Record<string, string> = {};
  try {
    const tok = (await cookies()).get("hive_jwt")?.value;
    if (tok) headers.cookie = `hive_jwt=${tok}`;
  } catch {
    // Outside a request scope (build-time prerender): nothing to forward.
  }
  // Bounded: an unreachable admin host must fail FAST with an honest error,
  // never hang the server component's render indefinitely — previously a bare
  // fetch with no timeout at all (ui-cloud-proxy-admin-fallback).
  const r = await fetch(`${ADMIN_OPS}${path}`, {
    cache: "no-store",
    headers,
    signal: AbortSignal.timeout(10_000),
  });
  if (!r.ok) throw new Error(`ops GET ${path} -> ${r.status}`);
  return (await r.json()) as T;
}

/**
 * `fetchOpsServer` that hands the failure back instead of throwing.
 *
 * Every admin server component used to write `.catch(() => null)`, which made a
 * REFUSED ops read indistinguishable from an empty one: with
 * `/v1/admin/databases` answering 403 the Databases page rendered "0 provisioned
 * · No databases provisioned" and no operator could tell a broken gate from an
 * empty fleet (2026-10-12). One primitive returns `{ data, error }` so every
 * page surfaces the refusal; callers that genuinely want to ignore a failure
 * still can, but they now have to say so.
 */
export type OpsResult<T> = { data: T | null; error: string | null };

export async function fetchOpsResult<T>(path: string): Promise<OpsResult<T>> {
  try {
    return { data: await fetchOpsServer<T>(path), error: null };
  } catch (e) {
    return { data: null, error: e instanceof Error ? e.message : String(e) };
  }
}
