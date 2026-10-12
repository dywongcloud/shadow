import "server-only";
import { cookies } from "next/headers";

// Server-side counterpart to lib/api.ts's opsGet -- used by admin ops pages'
// server components to fetch an initial paint of live operational data before
// streaming, avoiding the blank-shell-then-client-fetch round trip. Reads the
// SAME env vars as next.config.mjs's ADMIN_OPS rewrite target, so a server
// fetch here hits exactly what the browser's /ops/* proxy would have hit.
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const ADMIN_OPS = process.env.HIVE_ADMIN_OPS || ADMIN;
const INTERNAL = process.env.HIVE_INTERNAL_TOKEN || "";
// One address, not the list: the backend compares this claim against a single
// `admin_emails` entry when it derives `platform_admin`.
const OWNER_EMAIL = (process.env.HIVE_OWNER_EMAIL || "").split(",")[0].trim();

/* ---- The ops console's own credential -------------------------------------
 *
 * HIVE_INTERNAL_TOKEN authorizes TOKEN MINTING (`POST /v1/token`, proved by
 * `x-hive-internal`), not data reads: presenting it as a bearer (or as
 * `x-hive-internal`) on a data GET still answers 401 "missing or invalid bearer
 * token" (measured on the live DevHub admin listener, 2026-10-12). Forwarding
 * the operator's session cookie helped only when a request scope existed AND
 * that session carried `platform_admin` — with no cookie (or a tenant-scoped
 * one) every server-rendered ops read 401/403'd, so admin pages painted empty
 * shells and only the client poll ever showed data.
 *
 * So this process exchanges its internal token for a REAL platform JWT and
 * sends that as `Authorization: Bearer …`, falling back to the session cookie
 * when minting is unavailable. The mint is server-to-server only; this module is
 * `server-only` and the token never appears in a rendered payload, a client
 * bundle, or a response header — it exists solely in the headers of the fetch
 * below. The pages that call it are already gated on the platform-owner
 * allow-list by proxy.ts, so minting operator authority here grants nothing a
 * non-owner can reach.
 */
type Minted = { token: string; exp: number };
let minted: Minted | null = null;
let minting: Promise<string | null> | null = null;

async function mintOpsBearer(): Promise<string | null> {
  if (!INTERNAL) return null;
  try {
    const r = await fetch(`${ADMIN}/v1/token`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-hive-internal": INTERNAL },
      body: JSON.stringify({
        sub: "ui-ops-server",
        tenant: "personal",
        role: "owner",
        email: OWNER_EMAIL || undefined,
      }),
      cache: "no-store",
      signal: AbortSignal.timeout(10_000),
    });
    if (!r.ok) return null;
    const d = (await r.json()) as { token?: string; expires_in?: number };
    if (!d.token) return null;
    // Refresh well before the backend's own expiry so a read never races the
    // token's death.
    const ttl = Math.max(60, (typeof d.expires_in === "number" ? d.expires_in : 3600) - 120);
    minted = { token: d.token, exp: Date.now() + ttl * 1000 };
    return d.token;
  } catch {
    return null;
  }
}

/** Cached operator bearer. `force` re-mints (used after a 401/403, i.e. the
 *  cached JWT expired or the backend rotated its secret); concurrent callers
 *  share one in-flight mint instead of stampeding the token endpoint. */
function opsBearer(force = false): Promise<string | null> {
  if (!force && minted && Date.now() < minted.exp) return Promise.resolve(minted.token);
  if (!minting) {
    minting = mintOpsBearer().finally(() => {
      minting = null;
    });
  }
  return minting;
}

// Bounded: an unreachable admin host must fail FAST with an honest error, never
// hang the server component's render indefinitely — previously a bare fetch with
// no timeout at all (ui-cloud-proxy-admin-fallback).
function opsFetch(path: string, bearer: string | null, cookie: string | null): Promise<Response> {
  const headers: Record<string, string> = {};
  if (bearer) headers.authorization = `Bearer ${bearer}`;
  else if (cookie) headers.cookie = `hive_jwt=${cookie}`;
  return fetch(`${ADMIN_OPS}${path}`, {
    cache: "no-store",
    headers,
    signal: AbortSignal.timeout(10_000),
  });
}

/**
 * Fetch ops/admin data directly from the backend, server-side, AS AN OPERATOR.
 * Intentionally uncached (no-store): ops data is live operational state,
 * matching the existing no-store policy at the browser cache-control layer
 * (next.config.mjs headers()) -- never `"use cache"`.
 */
export async function fetchOpsServer<T>(path: string): Promise<T> {
  // Fallback credential: the operator's own session cookie, read inside the
  // request scope when there is one (a build-time prerender has none). The
  // minted bearer is preferred — it carries `platform_admin`, which a
  // tenant-scoped session cookie does not (witnessed 2026-10-01: 84
  // operator-403 lines in 2 h on the leader, all from this loopback caller).
  let cookie: string | null = null;
  try {
    cookie = (await cookies()).get("hive_jwt")?.value ?? null;
  } catch {
    /* outside a request scope: nothing to forward */
  }
  let bearer = await opsBearer();
  let r = await opsFetch(path, bearer, cookie);
  if ((r.status === 401 || r.status === 403) && bearer) {
    const fresh = await opsBearer(true);
    if (fresh && fresh !== bearer) r = await opsFetch(path, fresh, null);
  }
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
