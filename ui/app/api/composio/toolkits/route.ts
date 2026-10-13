import { NextResponse } from "next/server";
import { composioConfigured, listToolkits } from "@/lib/composio";

// Shared integrations-catalog cache: the backend admin-data collection `integration_index` (reported as source "guardian-db") plus the response header
// `Cache-Control: public, s-maxage=3600` (edge CDN + browser). The route stays dynamic (it reads the index per request and issues no-store fetches, which
// Next 16 rejects beside a route-segment `revalidate`), so the index and the header ARE the cache: the first load fetches Composio and indexes it, later
// loads read the index.
const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const INTERNAL = process.env.HIVE_INTERNAL_TOKEN || "";
/** The index collection is operator-gated for reads and writes, so the `_global` mint carries the owner email: ONE address, because the backend matches
 *  the claim against a single `admin_emails` entry while HIVE_OWNER_EMAIL is a comma-separated list. */
const FIRST_OWNER_EMAIL = (process.env.HIVE_OWNER_EMAIL || "").split(",")[0].trim();
const INDEX = "integration_index";

let _globalJwt: { token: string; expires: number } | null = null;
async function globalJwt(): Promise<string | null> {
  if (!INTERNAL) return null;
  if (_globalJwt && _globalJwt.expires > Date.now() + 30_000) return _globalJwt.token;
  try {
    const r = await fetch(`${ADMIN}/v1/token`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-hive-internal": INTERNAL },
      body: JSON.stringify({ sub: "system", tenant: "_global", role: "admin", email: FIRST_OWNER_EMAIL }),
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
    const jwt = await globalJwt();
    const r = await fetch(`${ADMIN}/v1/admin/data/${INDEX}?limit=1`, {
      cache: "no-store",
      headers: jwt ? { authorization: `Bearer ${jwt}` } : undefined,
    });
    if (!r.ok) return null;
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
  } catch {}
}

export async function GET() {
  const configured = composioConfigured();
  const headers = { "Cache-Control": "public, s-maxage=3600, stale-while-revalidate=86400" };

  const indexed = await readIndex();
  if (indexed) {
    return NextResponse.json({ configured, toolkits: indexed, source: "guardian-db" }, { headers });
  }

  if (!configured) {
    return NextResponse.json({ configured, toolkits: [], source: "none" }, { headers });
  }

  const toolkits = await listToolkits();
  if (toolkits.length) await writeIndex(toolkits);
  return NextResponse.json({ configured, toolkits, source: "composio" }, { headers });
}
