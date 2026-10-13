import { NextRequest, NextResponse } from "next/server";
import { resolveEntity, githubAccessToken } from "@/lib/github";
import { authTokenFrom, backend } from "@/lib/gitops-server";

const GITHUB_TOKEN_LOOKUP_DEADLINE_MS = 8_000;

/** Redeploy entry point (the Redeploy modal): like /api/git/deploy, attaches the signed-in user's connected-GitHub token SERVER-SIDE so a private repo can be cloned.
 *  The backend rebuilds `repo_url` from the project's newest deployment, so the token is attached unconditionally; the backend injects it only for https://github.com/
 *  clone URLs and drops it for zip and image sources. It is never returned to the browser. */
export async function POST(req: NextRequest, ctx: { params: Promise<{ project: string }> }) {
  const { project } = await ctx.params;
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const team = String((body.team as string) || req.headers.get("x-hive-team") || "personal");
  const token = await Promise.race([
    (async () => {
      try {
        return await githubAccessToken(await resolveEntity());
      } catch {
        return null;
      }
    })(),
    new Promise<null>((resolve) => setTimeout(() => resolve(null), GITHUB_TOKEN_LOOKUP_DEADLINE_MS)),
  ]);
  if (token) body.git_token = token;
  const r = await backend(
    `/v1/projects/${encodeURIComponent(project)}/redeploy`,
    team,
    { method: "POST", body: JSON.stringify(body) },
    authTokenFrom(req),
  );
  const text = await r.text();
  return new NextResponse(text, {
    status: r.status,
    headers: { "content-type": r.headers.get("content-type") || "application/json" },
  });
}
