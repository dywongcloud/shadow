import { NextRequest, NextResponse } from "next/server";
import { resolveEntity, toolkitStatus, connectionCredentials, composioConfigured } from "@/lib/composio";
import { authTokenFrom, backend } from "@/lib/gitops-server";
import { roleRefusalSentence } from "@/lib/team-role";

/** Toolkit slug + credential key -> ENV var name, e.g. ("google-sheets", "api_key") -> "GOOGLE_SHEETS_API_KEY". */
function envName(slug: string, key: string): string {
  const up = (s: string) =>
    s
      .replace(/([a-z0-9])([A-Z])/g, "$1_$2")
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
  const primaryCredential = credentials.access_token || credentials.token || credentials.bearer_token;
  if (primaryCredential) env[`${envName(slug, "")}TOKEN`.replace(/_+TOKEN$/, "_TOKEN")] = primaryCredential;
  return env;
}

/** Links a Composio-connected integration as a consumable platform resource, called after a toolkit OAuth completes (`?connected=<slug>`): registers the live
 *  credentials as a team-scoped integration (`POST /v1/integrations`) and auto-injects them as env vars into the team's projects, so apps (and the Vercel-compatible
 *  SDK, repointed at our API with a hive_ key) read them with no code. */
export async function POST(req: NextRequest) {
  if (!composioConfigured()) {
    return NextResponse.json({ ok: false, error: "Composio not configured (COMPOSIO_API_KEY)." }, { status: 400 });
  }
  const body = await req.json().catch(() => ({}));
  const slug: string = (body?.slug || "").trim().toLowerCase();
  const name: string = (body?.name || slug || "").trim();
  const team: string = (body?.team || req.headers.get("x-hive-team") || "personal").trim() || "personal";
  const onlyProject: string | undefined = body?.project ? String(body.project) : undefined;
  if (!slug) return NextResponse.json({ ok: false, error: "slug is required" }, { status: 400 });

  const authToken = authTokenFrom(req);
  const entity = await resolveEntity();

  const status = await toolkitStatus(entity, slug);
  if (!status.connected) {
    return NextResponse.json({ ok: false, error: `${slug} is not connected yet.` }, { status: 409 });
  }

  const cred = await connectionCredentials(entity, slug);
  if (!cred || Object.keys(cred.credentials).length === 0) {
    const bare = await backend("/v1/integrations", team, authToken, {
      method: "POST",
      body: JSON.stringify({ provider: slug, name, kind: cred?.kind || "oauth", entity, credentials: {}, env: {} }),
    }).catch(() => null);
    const bareRefusal = bare?.status === 403 ? roleRefusalSentence(await bare.text().catch(() => "")) : null;
    if (bareRefusal) return NextResponse.json({ ok: false, error: bareRefusal }, { status: 403 });
    return NextResponse.json({ ok: true, provider: slug, env: [], injected: 0, note: "connected; no extractable credentials" });
  }

  const env = envFor(slug, cred.credentials);

  const regRes = await backend("/v1/integrations", team, authToken, {
    method: "POST",
    body: JSON.stringify({ provider: slug, name, kind: cred.kind, entity, credentials: cred.credentials, env }),
  });
  if (!regRes.ok) {
    const refusal = regRes.status === 403 ? roleRefusalSentence(await regRes.text().catch(() => "")) : null;
    return NextResponse.json({ ok: false, error: refusal ?? `register failed (${regRes.status})` }, { status: refusal ? 403 : 502 });
  }
  const resource = await regRes.json().catch(() => ({}));

  let injected = 0;
  let injectionError: string | undefined;
  try {
    const projRes = await backend("/v1/gitops/projects", team, authToken);
    if (!projRes.ok) throw new Error(`projects read answered HTTP ${projRes.status}`);
    const projects: Array<{ project: string }> = await projRes.json();
    const targets = onlyProject ? projects.filter((p) => p.project === onlyProject) : projects;
    for (const p of targets) {
      for (const [key, value] of Object.entries(env)) {
        const r = await backend(`/v1/projects/${encodeURIComponent(p.project)}/env`, team, authToken, {
          method: "POST",
          body: JSON.stringify({ key, value, target: "production", sensitive: true }),
        });
        if (r.ok) injected++;
      }
    }
  } catch (e) {
    injectionError = e instanceof Error ? e.message : String(e);
  }

  return NextResponse.json({
    ok: true,
    provider: slug,
    resource: resource?.id || null,
    env: Object.keys(env),
    injected,
    ...(injectionError ? { injectionError } : {}),
  });
}
