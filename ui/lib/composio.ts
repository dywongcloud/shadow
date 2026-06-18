// Server-side Composio GitHub helper using the Composio **v3 REST API**.
// (The legacy `composio-core` SDK targets v1, which Composio has retired — it
// returns HTTP 410, so we call v3 directly via fetch.)
//
// Degrades gracefully when COMPOSIO_API_KEY is unset so manual git-URL import
// always works.
import "server-only";

const BASE = "https://backend.composio.dev/api/v3";
export const GITHUB_SLUG = "github";

export function composioConfigured(): boolean {
  return !!process.env.COMPOSIO_API_KEY;
}

function key(): string {
  return process.env.COMPOSIO_API_KEY || "";
}

async function v3(path: string, init?: RequestInit): Promise<any> {
  const r = await fetch(`${BASE}${path}`, {
    ...init,
    headers: { "x-api-key": key(), "content-type": "application/json", ...(init?.headers || {}) },
    cache: "no-store",
  });
  const text = await r.text();
  let body: any = {};
  try { body = text ? JSON.parse(text) : {}; } catch { body = { raw: text }; }
  if (!r.ok) {
    const msg = body?.error?.message || body?.message || `HTTP ${r.status}`;
    throw new Error(msg);
  }
  return body;
}

export interface Toolkit {
  slug: string;
  name: string;
  logo?: string;
  categories: string[];
  description?: string;
  no_auth: boolean;
}

/** List EVERY toolkit/integration available on Composio, following pagination. */
export async function listToolkits(): Promise<Toolkit[]> {
  if (!composioConfigured()) return [];
  try {
    const out: Toolkit[] = [];
    const seen = new Set<string>();
    let cursor: string | null = null;
    // Safety cap so a misbehaving cursor can't loop forever.
    for (let page = 0; page < 60; page++) {
      const qs = new URLSearchParams({ limit: "100" });
      if (cursor) qs.set("cursor", cursor);
      const res = await v3(`/toolkits?${qs.toString()}`);
      const items: any[] = res?.items ?? res?.toolkits ?? [];
      for (const t of items) {
        const slug = t?.slug;
        if (!slug || seen.has(slug)) continue;
        seen.add(slug);
        const rawCats = t?.meta?.categories ?? t?.categories ?? [];
        const categories: string[] = (Array.isArray(rawCats) ? rawCats : [])
          .map((c: any) => (typeof c === "string" ? c : c?.name))
          .filter(Boolean);
        const authSchemes: any[] = t?.auth_schemes ?? t?.meta?.auth_schemes ?? [];
        const no_auth = t?.no_auth === true || (Array.isArray(authSchemes) && authSchemes.length === 0);
        out.push({
          slug,
          name: t?.name || slug,
          logo: t?.meta?.logo || t?.logo,
          categories,
          description: t?.meta?.description || t?.description,
          no_auth,
        });
      }
      cursor = res?.next_cursor ?? res?.nextCursor ?? null;
      if (!cursor || items.length === 0 || out.length >= 2000) break;
    }
    return out;
  } catch (e) {
    console.error("composio listToolkits failed", e);
    return [];
  }
}

/** Find or create a Composio-managed auth config for a toolkit slug; return its id. */
async function toolkitAuthConfigId(slug: string): Promise<string> {
  const list = await v3(`/auth_configs?toolkit_slug=${encodeURIComponent(slug)}`);
  const items: any[] = list?.items ?? [];
  if (items[0]?.id) return items[0].id;
  const created = await v3(`/auth_configs`, {
    method: "POST",
    body: JSON.stringify({ toolkit: { slug }, auth_config: { type: "use_composio_managed_auth" } }),
  });
  return created?.auth_config?.id || created?.id;
}

/** Begin a connection for an arbitrary toolkit; returns a redirect URL (or error). */
export async function connectToolkit(
  entity: string,
  slug: string,
  redirectUrl: string
): Promise<{ redirectUrl?: string; error?: string }> {
  if (!composioConfigured()) return { error: "COMPOSIO_API_KEY not set" };
  try {
    const authConfigId = await toolkitAuthConfigId(slug);
    const conn = await v3(`/connected_accounts`, {
      method: "POST",
      body: JSON.stringify({
        auth_config: { id: authConfigId },
        connection: { user_id: entity, callback_url: redirectUrl },
      }),
    });
    const url = conn?.redirect_url || conn?.redirect_uri || conn?.connectionData?.val?.redirectUrl;
    // Some toolkits connect without an OAuth redirect (e.g. no_auth / API-key types).
    if (url) return { redirectUrl: url };
    const status = (conn?.status || "").toUpperCase();
    if (status === "ACTIVE" || status === "INITIATED") return { redirectUrl };
    return { error: "Composio returned no redirect URL." };
  } catch (e: any) {
    console.error("composio connectToolkit failed", e);
    return { error: `Composio: ${e?.message || "unknown error"}` };
  }
}

/** Whether this entity has an active connection for the given toolkit slug. */
export async function toolkitStatus(entity: string, slug: string): Promise<{ connected: boolean }> {
  if (!composioConfigured()) return { connected: false };
  try {
    const res = await v3(
      `/connected_accounts?user_ids=${encodeURIComponent(entity)}&toolkit_slugs=${encodeURIComponent(slug)}`
    );
    const items: any[] = res?.items ?? [];
    const connected = items.some((x) => (x.status || "").toUpperCase() === "ACTIVE");
    return { connected };
  } catch {
    return { connected: false };
  }
}

export interface GhRepo {
  name: string;
  full_name: string;
  clone_url: string;
  default_branch: string;
  private: boolean;
  updated_at?: string;
  owner?: string;
}

/** Find or create a Composio-managed GitHub auth config; return its id. */
async function githubAuthConfigId(): Promise<string> {
  const list = await v3(`/auth_configs?toolkit_slug=${GITHUB_SLUG}`);
  const items: any[] = list?.items ?? [];
  if (items[0]?.id) return items[0].id;
  const created = await v3(`/auth_configs`, {
    method: "POST",
    body: JSON.stringify({ toolkit: { slug: GITHUB_SLUG }, auth_config: { type: "use_composio_managed_auth" } }),
  });
  return created?.auth_config?.id || created?.id;
}

/** Whether this entity has an active GitHub connection. */
export async function githubStatus(entity: string): Promise<{ connected: boolean; configured: boolean }> {
  if (!composioConfigured()) return { connected: false, configured: false };
  try {
    const res = await v3(`/connected_accounts?user_ids=${encodeURIComponent(entity)}&toolkit_slugs=${GITHUB_SLUG}`);
    const items: any[] = res?.items ?? [];
    const connected = items.some((x) => (x.status || "").toUpperCase() === "ACTIVE");
    return { connected, configured: true };
  } catch {
    return { connected: false, configured: true };
  }
}

/** Begin an OAuth connection; returns a redirect URL (or an error message). */
export async function githubConnect(
  entity: string,
  redirectUrl: string
): Promise<{ redirectUrl?: string; error?: string }> {
  if (!composioConfigured()) return { error: "COMPOSIO_API_KEY not set" };
  try {
    const authConfigId = await githubAuthConfigId();
    const conn = await v3(`/connected_accounts`, {
      method: "POST",
      body: JSON.stringify({
        auth_config: { id: authConfigId },
        connection: { user_id: entity, callback_url: redirectUrl },
      }),
    });
    const url = conn?.redirect_url || conn?.redirect_uri || conn?.connectionData?.val?.redirectUrl;
    return url ? { redirectUrl: url } : { error: "Composio returned no redirect URL." };
  } catch (e: any) {
    console.error("composio connect failed", e);
    return { error: `Composio: ${e?.message || "unknown error"}` };
  }
}

/** List the connected GitHub user's repositories for this entity. */
export async function githubRepos(entity: string): Promise<GhRepo[]> {
  if (!composioConfigured()) return [];
  try {
    const res = await v3(`/tools/execute/GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER`, {
      method: "POST",
      body: JSON.stringify({ user_id: entity, arguments: { per_page: 100, sort: "updated" } }),
    });
    const data = res?.data?.details ?? res?.data ?? res ?? [];
    const arr: any[] = Array.isArray(data) ? data : data?.repositories ?? data?.items ?? [];
    return arr.map((r: any) => ({
      name: r.name,
      full_name: r.full_name,
      clone_url: r.clone_url || `https://github.com/${r.full_name}.git`,
      default_branch: r.default_branch || "main",
      private: !!r.private,
      updated_at: r.updated_at,
      owner: r.owner?.login,
    }));
  } catch (e) {
    console.error("composio repos failed", e);
    return [];
  }
}
