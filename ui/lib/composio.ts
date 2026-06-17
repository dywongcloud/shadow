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
