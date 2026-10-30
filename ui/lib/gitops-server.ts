import "server-only";
import { createHash } from "crypto";
import { cookies } from "next/headers";
import { buildArtifacts } from "@/lib/gitops-yaml";
import { teamRoleOf, type TeamRole } from "@/lib/team-role";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";
const BACKEND_TIMEOUT_MS = 20_000;

/** Calls the backend admin API scoped to a tenant. `authToken` is REQUIRED so no call site can forget it: pass the caller's platform JWT (forwarded as
 *  `Authorization: Bearer`; a Next route's server->backend `fetch()` does not carry the browser's `hive_jwt` cookie, and the backend derives the tenant from
 *  the JWT claim, not `x-hive-team`), or an explicit `null` only where an anonymous read is intended (an auth-enforced node answers it as the empty anonymous tenant). */
export async function backend(path: string, team: string, authToken: string | null, init?: RequestInit) {
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(), BACKEND_TIMEOUT_MS);
  try {
    return await fetch(`${ADMIN}${path}`, {
      ...init,
      headers: {
        "content-type": "application/json",
        "x-hive-team": team,
        ...(authToken ? { authorization: `Bearer ${authToken}` } : {}),
        ...(init?.headers || {}),
      },
      cache: "no-store",
      signal: ctrl.signal,
    });
  } finally {
    clearTimeout(t);
  }
}

/** Extract the caller's platform JWT from a Next request (httpOnly `hive_jwt`
 *  cookie, or an incoming `Authorization: Bearer`) to forward to the backend. */
export function authTokenFrom(req: {
  cookies?: { get(name: string): { value: string } | undefined };
  headers: { get(name: string): string | null };
}): string | null {
  const cookie = req.cookies?.get?.("hive_jwt")?.value;
  if (cookie) return cookie;
  const bearer = req.headers.get("authorization");
  if (bearer && /^Bearer\s+/i.test(bearer)) return bearer.replace(/^Bearer\s+/i, "").trim() || null;
  return null;
}

/** The team role and operator status a platform JWT claims, read WITHOUT verifying it: a server route uses them only to refuse an action BEFORE a side effect the backend would refuse anyway (a GitHub repository created for a link
 *  the matrix rejects); the backend verifies the token and stays the authority. `role: null` = absent or undecodable = UNKNOWN: the route proceeds and the backend answers. */
export function claimedRole(token: string | null): { role: TeamRole | null; operator: boolean } {
  try {
    const part = token?.split(".")[1];
    if (!part) return { role: null, operator: false };
    const claims = JSON.parse(Buffer.from(part, "base64url").toString("utf8")) as { role?: unknown; platform_admin?: unknown };
    return { role: teamRoleOf(typeof claims.role === "string" ? claims.role : null), operator: claims.platform_admin === true };
  } catch {
    return { role: null, operator: false };
  }
}

/** The signed-in user's platform JWT for a Server Component (the httpOnly `hive_jwt` cookie), or null when the request carries none. */
export async function sessionAuthToken(): Promise<string | null> {
  return (await cookies()).get("hive_jwt")?.value ?? null;
}

/** Reads the live platform objects for a tenant and renders the full GitOps artifact tree plus a content hash (ignoring volatile timestamps) so callers
 *  can skip a redundant commit. `failures` lists CORE reads that failed (only `/v1/gitops/projects` is core): committing a tree without projects would
 *  TOMBSTONE every real project file under the `projects/` managed prefix, so callers must refuse to commit when it is non-empty; the other reads
 *  degrade to absent-by-design (a personal namespace has no team row, the enterprise endpoints are optional). */
export async function buildOrgArtifacts(team: string, authToken: string | null, rootPath = "openedge.yaml") {
  const [projRes, teamRes, ovRes, routingRes, ipRes, siemRes, samlRes, scimRes, mfeRes, dbRes] = await Promise.all([
    backend("/v1/gitops/projects", team, authToken),
    backend(`/v1/teams/${encodeURIComponent(team)}`, team, authToken),
    backend("/v1/overview", team, authToken),
    backend("/v1/routing", team, authToken),
    backend("/v1/enterprise/ip-blocks", team, authToken),
    backend("/v1/enterprise/siem", team, authToken),
    backend("/v1/enterprise/saml", team, authToken),
    backend("/v1/enterprise/scim", team, authToken),
    backend("/v1/enterprise/microfrontends", team, authToken),
    backend("/v1/databases", team, authToken),
  ]);
  const failures: string[] = [];
  if (!projRes.ok) failures.push(`/v1/gitops/projects -> HTTP ${projRes.status}`);
  const projects = projRes.ok ? await projRes.json() : [];
  const teamInfo = teamRes.ok ? await teamRes.json() : null;
  const ov = ovRes.ok ? await ovRes.json() : null;
  const routing = routingRes.ok ? await routingRes.json().catch(() => null) : null;
  const ipBlocks = ipRes.ok ? (await ipRes.json().catch(() => null))?.blocks : null;
  const siem = siemRes.ok ? await siemRes.json().catch(() => null) : null;
  const saml = samlRes.ok ? await samlRes.json().catch(() => null) : null;
  const scim = scimRes.ok ? await scimRes.json().catch(() => null) : null;
  const mfe = mfeRes.ok ? (await mfeRes.json().catch(() => null))?.groups : null;
  const databases = dbRes.ok ? await dbRes.json().catch(() => null) : null;

  const files = buildArtifacts({
    org: {
      name: teamInfo?.name || (team === "personal" ? "Personal" : team),
      slug: team,
      plan: teamInfo?.plan,
    },
    generatedAt: new Date().toISOString(),
    region: ov?.region,
    rootPath,
    projects: Array.isArray(projects) ? projects : [],
    databases: Array.isArray(databases) ? databases : [],
    platform: {
      routing: routing ? { redirects: routing.redirects || [], rewrites: routing.rewrites || [] } : undefined,
      enterprise: {
        ipBlocks: Array.isArray(ipBlocks) ? ipBlocks.map((b: any) => ({ prefix: b.prefix, note: b.note || undefined })) : [],
        siem: { enabled: !!siem?.enabled, format: siem?.format },
        saml: { enabled: !!saml?.enabled, enforced: !!saml?.enforced },
        scim: { enabled: !!scim?.enabled },
        microfrontends: Array.isArray(mfe)
          ? mfe.map((g: any) => ({
              name: g.name,
              host: g.host_project,
              children: (g.children || []).map((ch: any) => ({ project: ch.project, path: ch.path_prefix })),
            }))
          : [],
      },
    },
  });

  const contentWithoutTimestamps = files
    .map((f) => `${f.path}\n${f.content}`)
    .join("\n")
    .replace(/^\s*generatedAt:.*$/gm, "")
    .replace(/\d{4}-\d{2}-\d{2}T[\d:.]+Z/g, "");
  const hash = createHash("sha256").update(contentWithoutTimestamps).digest("hex").slice(0, 16);

  return { files, hash, projectCount: Array.isArray(projects) ? projects.length : 0, failures };
}
