import "server-only";
import { cookies } from "next/headers";

// Server-side counterpart to lib/api.ts's opsGet for admin server components: an initial paint of live ops data without a client round trip, reading
// the SAME env vars as next.config.mjs's ADMIN_OPS rewrite so it hits exactly what the browser's /ops proxy would.
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const ADMIN_OPS = process.env.HIVE_ADMIN_OPS || ADMIN;
const INTERNAL = process.env.HIVE_INTERNAL_TOKEN || "";
const FIRST_OWNER_EMAIL = (process.env.HIVE_OWNER_EMAIL || "").split(",")[0].trim();
const OPS_REQUEST_TIMEOUT_MS = 10_000;
const TOKEN_TTL_FALLBACK_SECS = 3600;
const RETIRE_BEFORE_EXPIRY_SECS = 120;
const MIN_TOKEN_TTL_SECS = 60;

function unrefTimer(timer: ReturnType<typeof setTimeout>): void {
  (timer as unknown as { unref?: () => void }).unref?.();
}

type Minted = { token: string };
let minted: Minted | null = null;
let expiry: ReturnType<typeof setTimeout> | null = null;
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
        email: FIRST_OWNER_EMAIL || undefined,
      }),
      cache: "no-store",
      signal: AbortSignal.timeout(OPS_REQUEST_TIMEOUT_MS),
    });
    if (!r.ok) return null;
    const d = (await r.json()) as { token?: string; expires_in?: number };
    if (!d.token) return null;
    const ttl = Math.max(
      MIN_TOKEN_TTL_SECS,
      (typeof d.expires_in === "number" ? d.expires_in : TOKEN_TTL_FALLBACK_SECS) - RETIRE_BEFORE_EXPIRY_SECS,
    );
    minted = { token: d.token };
    if (expiry) clearTimeout(expiry);
    expiry = setTimeout(() => {
      minted = null;
      expiry = null;
    }, ttl * 1000);
    unrefTimer(expiry);
    return d.token;
  } catch {
    return null;
  }
}

/** The ops console's own credential: HIVE_INTERNAL_TOKEN authorizes token MINTING only (`POST /v1/token`, `x-hive-internal`), not data GETs (as a
 *  bearer it still 401s), so this server-only module exchanges it for a real platform JWT sent as `Authorization: Bearer` (the session cookie is the
 *  fallback); the token never reaches a rendered payload, client bundle or response header. Cached; `force` re-mints after a 401/403, concurrent
 *  callers share one in-flight mint, and expiry is a TIMER, never a `Date.now()` read (which fails a Cache Components prerender). */
function opsBearer(force = false): Promise<string | null> {
  if (!force && minted) return Promise.resolve(minted.token);
  if (!minting) {
    minting = mintOpsBearer().finally(() => {
      minting = null;
    });
  }
  return minting;
}

function opsFetch(path: string, bearer: string | null, cookie: string | null): Promise<Response> {
  const headers: Record<string, string> = {};
  if (bearer) headers.authorization = `Bearer ${bearer}`;
  else if (cookie) headers.cookie = `hive_jwt=${cookie}`;
  return fetch(`${ADMIN_OPS}${path}`, {
    cache: "no-store",
    headers,
    signal: AbortSignal.timeout(OPS_REQUEST_TIMEOUT_MS),
  });
}

/** Fetches ops/admin data server-side AS AN OPERATOR, uncached (no-store: live operational state, matching the browser cache-control layer; never
 *  `"use cache"`). It carries real operator authority, so call it only from pages behind proxy.ts's owner allow-list (`/admin`, `/ops`). */
export async function fetchOpsServer<T>(path: string): Promise<T> {
  let cookie: string | null = null;
  try {
    cookie = (await cookies()).get("hive_jwt")?.value ?? null;
  } catch {}
  let bearer = await opsBearer();
  let r = await opsFetch(path, bearer, cookie);
  if ((r.status === 401 || r.status === 403) && bearer) {
    const fresh = await opsBearer(true);
    if (fresh && fresh !== bearer) r = await opsFetch(path, fresh, null);
  }
  if (!r.ok) throw new Error(`ops GET ${path} -> ${r.status}`);
  return (await r.json()) as T;
}

/** `fetchOpsServer` returning `{ data, error }` instead of throwing, so a REFUSED ops read is never rendered as an empty one (a bare
 *  `.catch(() => null)` made a 403 look like "0 provisioned"); a caller that wants to ignore a failure has to say so. */
export type OpsResult<T> = { data: T | null; error: string | null };

export async function fetchOpsResult<T>(path: string): Promise<OpsResult<T>> {
  try {
    return { data: await fetchOpsServer<T>(path), error: null };
  } catch (e) {
    return { data: null, error: e instanceof Error ? e.message : String(e) };
  }
}
