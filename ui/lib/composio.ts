// Server-side Composio GitHub helper (multi-tenant via entity id).
// Degrades gracefully when COMPOSIO_API_KEY is unset so the dashboard still
// works (manual git-URL import is always available).
import "server-only";
import { cookies } from "next/headers";

export const GITHUB_APP = "github";

export function composioConfigured(): boolean {
  return !!process.env.COMPOSIO_API_KEY;
}

/** Stable per-browser entity id (the multi-tenant key Composio routes on). */
export function entityId(): string {
  const c = cookies();
  let id = c.get("hive_entity")?.value;
  if (!id) {
    id = "user-" + Math.random().toString(36).slice(2, 10);
    // Note: set via response in the route; here we just fall back to a value.
  }
  return id || "default";
}

// composio-core is CJS; import lazily so a missing/var SDK never breaks the app.
async function client() {
  const { Composio } = await import("composio-core");
  return new Composio({ apiKey: process.env.COMPOSIO_API_KEY });
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

/** Whether this entity has an active GitHub connection. */
export async function githubStatus(entity: string): Promise<{ connected: boolean; configured: boolean }> {
  if (!composioConfigured()) return { connected: false, configured: false };
  try {
    const c = await client();
    const conns: any = await (c as any).connectedAccounts.list({ entityId: entity });
    const items = conns?.items ?? conns ?? [];
    const connected = Array.isArray(items)
      ? items.some((x: any) => (x.appName || x.appUniqueId || "").toLowerCase().includes("github"))
      : false;
    return { connected, configured: true };
  } catch {
    return { connected: false, configured: true };
  }
}

/** Begin an OAuth connection; returns a redirect URL to send the user to. */
export async function githubConnect(entity: string, redirectUrl: string): Promise<string | null> {
  if (!composioConfigured()) return null;
  try {
    const c = await client();
    const entityObj: any = await (c as any).getEntity(entity);
    const conn: any = await entityObj.initiateConnection({
      appName: GITHUB_APP,
      redirectUrl,
    });
    return conn?.redirectUrl || conn?.connectionStatus?.redirectUrl || null;
  } catch (e) {
    console.error("composio connect failed", e);
    return null;
  }
}

/** List the connected GitHub user's repositories for this entity. */
export async function githubRepos(entity: string): Promise<GhRepo[]> {
  if (!composioConfigured()) return [];
  try {
    const c = await client();
    const res: any = await (c as any).getEntity(entity).then((e: any) =>
      e.execute({ action: "GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER", params: {} })
    );
    const data = res?.data?.details ?? res?.data ?? res ?? [];
    const arr: any[] = Array.isArray(data) ? data : data?.repositories ?? [];
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
