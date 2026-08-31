// Link a Composio-connected integration as a consumable platform resource.
//
// Called right after a toolkit OAuth completes (the Integrations tab returns with
// `?connected=<slug>`). It pulls the live connection credentials from Composio,
// registers them as a team-scoped integration resource on the platform
// (`POST /v1/integrations`), and AUTO-INJECTS them as env vars into the team's
// project deployments — so apps consume the resource with zero code, and the
// Vercel-compatible SDK (repointed at our API + a hive_ key) can read them back.

import { NextRequest, NextResponse } from "next/server";
import { resolveEntity, toolkitStatus, connectionCredentials, composioConfigured } from "@/lib/composio";
import { authTokenFrom, backend } from "@/lib/gitops-server";

/** Normalize a toolkit slug + credential key into an ENV var name, e.g.
 *  ("google-sheets", "api_key") -> "GOOGLE_SHEETS_API_KEY". */
function envName(slug: string, key: string): string {
  const up = (s: string) =>
    s
      .replace(/([a-z0-9])([A-Z])/g, "$1_$2") // camelCase -> snake
      .replace(/[^a-zA-Z0-9]+/g, "_")
      .replace(/^_+|_+$/g, "")
      .toUpperCase();
  return `${up(slug)}_${up(key)}`;
}

/** Map a connection's credentials to the env vars we inject + store. */
function envFor(slug: string, credentials: Record<string, string>): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [k, v] of Object.entries(credentials)) {
    if (typeof v === "string" && v) env[envName(slug, k)] = v;
  }
  // Convenience alias: <SLUG>_TOKEN for the primary OAuth/bearer credential.
  const primary = credentials.access_token || credentials.token || credentials.bearer_token;
  if (primary) env[`${envName(slug, "")}TOKEN`.replace(/_+TOKEN$/, "_TOKEN")] = primary;
  return env;
}

export async function POST(req: NextRequest) {
  if (!composioConfigured()) {
    return NextResponse.json({ ok: false, error: "Composio not configured (COMPOSIO_API_KEY)." }, { status: 400 });
  }
  const body = await req.json().catch(() => ({}));
  const slug: string = (body?.slug || "").trim().toLowerCase();
  const name: string = (body?.name || slug || "").trim();
  const team: string = (body?.team || req.headers.get("x-hive-team") || "personal").trim() || "personal";
  // Optional: restrict injection to one project (default = all of the team's).
  const onlyProject: string | undefined = body?.project ? String(body.project) : undefined;
  if (!slug) return NextResponse.json({ ok: false, error: "slug is required" }, { status: 400 });

  const entity = await resolveEntity();

  // The integration must actually be connected (active OAuth/api-key connection).
  const status = await toolkitStatus(entity, slug);
  if (!status.connected) {
    return NextResponse.json({ ok: false, error: `${slug} is not connected yet.` }, { status: 409 });
  }

  const cred = await connectionCredentials(entity, slug);
  if (!cred || Object.keys(cred.credentials).length === 0) {
    // Connected but no readable credential (some toolkits manage auth opaquely).
    // Still register the link so it shows as connected; just no env to inject.
    await backend("/v1/integrations", team, {
      method: "POST",
      body: JSON.stringify({ provider: slug, name, kind: cred?.kind || "oauth", entity, credentials: {}, env: {} }),
    }, authTokenFrom(req)).catch(() => {});
    return NextResponse.json({ ok: true, provider: slug, env: [], injected: 0, note: "connected; no extractable credentials" });
  }

  const env = envFor(slug, cred.credentials);

  // 1) Register the integration resource (team-scoped; consumable via SDK/api key).
  const regRes = await backend("/v1/integrations", team, {
    method: "POST",
    body: JSON.stringify({ provider: slug, name, kind: cred.kind, entity, credentials: cred.credentials, env }),
  }, authTokenFrom(req));
  if (!regRes.ok) {
    return NextResponse.json({ ok: false, error: `register failed (${regRes.status})` }, { status: 502 });
  }
  const resource = await regRes.json().catch(() => ({}));

  // 2) Auto-inject env vars into the team's project deployments.
  let injected = 0;
  try {
    const projRes = await backend("/v1/gitops/projects", team);
    const projects: Array<{ project: string }> = projRes.ok ? await projRes.json() : [];
    const targets = onlyProject ? projects.filter((p) => p.project === onlyProject) : projects;
    for (const p of targets) {
      for (const [key, value] of Object.entries(env)) {
        const r = await backend(`/v1/projects/${encodeURIComponent(p.project)}/env`, team, {
          method: "POST",
          body: JSON.stringify({ key, value, target: "production", sensitive: true }),
        }, authTokenFrom(req));
        if (r.ok) injected++;
      }
    }
  } catch {
    /* projects unavailable — the resource is still registered + SDK-readable */
  }

  return NextResponse.json({
    ok: true,
    provider: slug,
    resource: resource?.id || null,
    env: Object.keys(env),
    injected,
  });
}
