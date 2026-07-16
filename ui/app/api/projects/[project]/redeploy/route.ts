import { NextRequest, NextResponse } from "next/server";
import { resolveEntity, githubAccessToken } from "@/lib/github";
import { backend } from "@/lib/gitops-server";

export const dynamic = "force-dynamic";

/**
 * Redeploy entry point (the Vercel-style "Redeploy" modal). Same purpose as
 * /api/git/deploy: attach the signed-in user's connected-GitHub token SERVER-SIDE so
 * a redeploy of a PRIVATE git repo can clone. The backend reconstructs the repo_url
 * from the project's newest deployment, so we can't cheaply tell here whether it's a
 * github repo — we attach the token unconditionally (the backend only injects it for
 * https://github.com/ clone URLs and ignores it for zip/image redeploys). The token
 * is never returned to the browser.
 */
export async function POST(req: NextRequest, ctx: { params: Promise<{ project: string }> }) {
  const { project } = await ctx.params;
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const team = String((body.team as string) || req.headers.get("x-hive-team") || "personal");
  // Bounded so a slow/unreachable Composio never hangs the redeploy — fall back to
  // no token (backend then uses the GITHUB_TOKEN env fallback or a clear error).
  const token = await Promise.race([
    (async () => {
      try {
        return await githubAccessToken(await resolveEntity());
      } catch {
        return null;
      }
    })(),
    new Promise<null>((resolve) => setTimeout(() => resolve(null), 8000)),
  ]);
  if (token) body.git_token = token;
  const r = await backend(`/v1/projects/${encodeURIComponent(project)}/redeploy`, team, {
    method: "POST",
    body: JSON.stringify(body),
  });
  const text = await r.text();
  return new NextResponse(text, {
    status: r.status,
    headers: { "content-type": r.headers.get("content-type") || "application/json" },
  });
}
