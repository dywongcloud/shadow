import { NextResponse } from "next/server";
import { composioConfigured, listToolkits } from "@/lib/composio";

// The cross-user/cross-device shared cache of the integrations catalog is the
// platform's guardian-db (docstore) collection `integration_index`, PLUS the
// response `Cache-Control: public, s-maxage=3600` header (honored by the edge
// CDN + browser). Both are framework-independent, so they carry the exact same
// behavior across the Next 16 upgrade:
//   • first ever load (no collection) → fetch from Composio + index it
//   • every load after → pull the stored index from guardian-db (no Composio hit)
// The route itself is dynamic: it reads guardian-db per request (the cache
// lookup) and issues no-store fetches, so — unlike Next 14's route-segment
// `revalidate` (which Next 16 rejects as a conflict with a no-store fetch) — the
// route is not Full-Route-Cached; the guardian-db + s-maxage header ARE the cache.
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const INTERNAL = process.env.HIVE_INTERNAL_TOKEN || "";
// The global integration-index collection is operator-gated (data_create ->
// require_operator), so the mint must carry the owner email for the backend to
// derive platform-operator authority. Server-only; never exposed to the browser.
const OWNER_EMAIL = process.env.HIVE_OWNER_EMAIL || "";
const INDEX = "integration_index";

// The index write is a MUTATION on the backend admin API, which require_auth
// gates when HIVE_JWT_SECRET is set — so without a token the cache never
// persisted and every marketplace load re-fetched Composio's full catalog. This
// is a GLOBAL (non-user) cache, so mint a short-lived `_global` platform JWT with
// the server's internal token (the same mint /api/token uses) and Bearer it.
let _globalJwt: { token: string; expires: number } | null = null;
async function globalJwt(): Promise<string | null> {
  if (!INTERNAL) return null; // unenforced/dev backend — no token needed
  if (_globalJwt && _globalJwt.expires > Date.now() + 30_000) return _globalJwt.token;
  try {
    const r = await fetch(`${ADMIN}/v1/token`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-hive-internal": INTERNAL },
      body: JSON.stringify({ sub: "system", tenant: "_global", role: "admin", email: OWNER_EMAIL }),
      cache: "no-store",
    });
    if (!r.ok) return null;
    const d = await r.json();
    if (!d?.token) return null;
    _globalJwt = { token: d.token, expires: Date.now() + (d.expires_in || 3600) * 1000 };
    return d.token;
  } catch {
    return null;
  }
}

async function readIndex(): Promise<any[] | null> {
  try {
    // The admin-data collection is operator-gated (read too), so carry the
    // minted _global operator JWT — without it the read 403'd and the cache
    // always missed (falling back to a full Composio re-fetch every load).
    const jwt = await globalJwt();
    const r = await fetch(`${ADMIN}/v1/admin/data/${INDEX}?limit=1`, {
      cache: "no-store",
      headers: jwt ? { authorization: `Bearer ${jwt}` } : undefined,
    });
    if (!r.ok) return null; // 404 = collection doesn't exist yet
    const body = await r.json();
    const row = (body?.rows ?? [])[0];
    const toolkits = row?.toolkits;
    return Array.isArray(toolkits) && toolkits.length ? toolkits : null;
  } catch {
    return null;
  }
}

async function writeIndex(toolkits: any[]): Promise<void> {
  try {
    const jwt = await globalJwt();
    await fetch(`${ADMIN}/v1/admin/data/${INDEX}`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-hive-team": "_global",
        ...(jwt ? { authorization: `Bearer ${jwt}` } : {}),
      },
      body: JSON.stringify({ kind: "integration_index", count: toolkits.length, toolkits }),
    });
  } catch {
    /* best-effort: indexing failure shouldn't break the listing */
  }
}

export async function GET() {
  const configured = composioConfigured();
  const headers = { "Cache-Control": "public, s-maxage=3600, stale-while-revalidate=86400" };

  // 1) Serve from the guardian-db index if it's already been built.
  const indexed = await readIndex();
  if (indexed) {
    return NextResponse.json({ configured, toolkits: indexed, source: "guardian-db" }, { headers });
  }

  if (!configured) {
    return NextResponse.json({ configured, toolkits: [], source: "none" }, { headers });
  }

  // 2) First load: pull from Composio and index into guardian-db for next time.
  const toolkits = await listToolkits();
  if (toolkits.length) await writeIndex(toolkits);
  return NextResponse.json({ configured, toolkits, source: "composio" }, { headers });
}
