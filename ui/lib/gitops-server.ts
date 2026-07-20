import "server-only";
import { createHash } from "crypto";
import { buildArtifacts } from "@/lib/gitops-yaml";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";

/**
 * Call the backend admin API scoped to a tenant.
 *
 * `authToken` (the signed-in user's platform JWT) is forwarded as
 * `Authorization: Bearer …` so MUTATIONS pass the backend's `require_auth`
 * (when `HIVE_JWT_SECRET` is set). A Next server route that POSTs to the backend
 * on the user's behalf (deploy, redeploy, GitOps writes) MUST pass it — unlike
 * the `/cloud` rewrite proxy, a server→backend `fetch()` does not carry the
 * browser's `hive_jwt` cookie automatically, so omitting it 401s with
 * "missing or invalid bearer token". Reads (no token) stay unauthenticated, and
 * the backend derives the tenant from the JWT claim (not the spoofable
 * `x-hive-team` header), so forwarding the user's token scopes writes correctly.
 */
// Mirrors lib/api.ts's fetchWithTimeout: an unbounded server->backend fetch()
// here (GitHub API calls in commitFiles() share the same failure shape) can
// stall the whole /api/gitops/sync request indefinitely on a hung backend or
// far region, which surfaces client-side as a raw "Failed to fetch" instead of
// a clean, retryable HTTP error. 20s matches apiGet's own default timeout.
const BACKEND_TIMEOUT_MS = 20_000;

export async function backend(path: string, team: string, init?: RequestInit, authToken?: string | null) {
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

/**
 * Read the live platform objects for a tenant and render the full GitOps artifact
 * tree. Returns the files plus a content hash (ignoring the volatile generatedAt
 * stamps) so callers can skip a redundant commit when nothing changed.
 */
export async function buildOrgArtifacts(team: string, rootPath = "openedge.yaml", authToken?: string | null) {
  const [projRes, teamRes, ovRes, routingRes, ipRes, siemRes, samlRes, scimRes, mfeRes, dbRes] = await Promise.all([
    backend("/v1/gitops/projects", team, undefined, authToken),
    backend(`/v1/teams/${encodeURIComponent(team)}`, team, undefined, authToken),
    backend("/v1/overview", team, undefined, authToken),
    backend("/v1/routing", team, undefined, authToken),
    backend("/v1/enterprise/ip-blocks", team, undefined, authToken),
    backend("/v1/enterprise/siem", team, undefined, authToken),
    backend("/v1/enterprise/saml", team, undefined, authToken),
    backend("/v1/enterprise/scim", team, undefined, authToken),
    backend("/v1/enterprise/microfrontends", team, undefined, authToken),
    // Same tenant-scoped endpoint the Storage dashboard page polls
    // (ui/app/storage/page.tsx: usePoll<Database[]>("/v1/databases", …)) —
    // reused here rather than a bespoke gitops-only route.
    backend("/v1/databases", team, undefined, authToken),
  ]);
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
    // Raw records pass through — declarative-shape extraction (name/kind/region/
    // mode only, never `connection`/credentials) happens in buildArtifacts's
    // databaseNode(), mirroring how projectNode() shapes `projects` above.
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

  // Hash over all files with volatile timestamps neutralized so an unchanged
  // config doesn't re-commit on every poll. Strip BOTH the YAML `generatedAt:`
  // lines AND any ISO-8601 timestamp anywhere (e.g. the README "Generated …").
  const stable = files
    .map((f) => `${f.path}\n${f.content}`)
    .join("\n")
    .replace(/^\s*generatedAt:.*$/gm, "")
    .replace(/\d{4}-\d{2}-\d{2}T[\d:.]+Z/g, "");
  const hash = createHash("sha256").update(stable).digest("hex").slice(0, 16);

  return { files, hash, projectCount: Array.isArray(projects) ? projects.length : 0 };
}
