import "server-only";
import { createHash } from "crypto";
import { buildArtifacts } from "@/lib/gitops-yaml";

const ADMIN = process.env.HIVE_ADMIN || "http://127.0.0.1:8786";

/** Call the backend admin API scoped to a tenant. */
export async function backend(path: string, team: string, init?: RequestInit) {
  return fetch(`${ADMIN}${path}`, {
    ...init,
    headers: { "content-type": "application/json", "x-hive-team": team, ...(init?.headers || {}) },
    cache: "no-store",
  });
}

/**
 * Read the live platform objects for a tenant and render the full GitOps artifact
 * tree. Returns the files plus a content hash (ignoring the volatile generatedAt
 * stamps) so callers can skip a redundant commit when nothing changed.
 */
export async function buildOrgArtifacts(team: string, rootPath = "openedge.yaml") {
  const [projRes, teamRes, ovRes, routingRes, ipRes, siemRes, samlRes, scimRes, mfeRes] = await Promise.all([
    backend("/v1/gitops/projects", team),
    backend(`/v1/teams/${encodeURIComponent(team)}`, team),
    backend("/v1/overview", team),
    backend("/v1/routing", team),
    backend("/v1/enterprise/ip-blocks", team),
    backend("/v1/enterprise/siem", team),
    backend("/v1/enterprise/saml", team),
    backend("/v1/enterprise/scim", team),
    backend("/v1/enterprise/microfrontends", team),
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
