import { NextRequest, NextResponse } from "next/server";
import { resolveEntity, githubAccessToken } from "@/lib/github";
import { authTokenFrom, backend } from "@/lib/gitops-server";

export const dynamic = "force-dynamic";

/**
 * Deploy-from-Git entry point. The dashboard posts the deploy body here (instead of
 * straight to the backend `/v1/git/deploy`) so we can attach the signed-in user's
 * connected-GitHub OAuth token SERVER-SIDE for a PRIVATE-repo clone. The token is
 * fetched from Composio (per-user entity), placed on `body.git_token`, and forwarded
 * to the backend — it is NEVER returned to the browser. Public repos / no connection
 * simply carry no token and clone anonymously (the backend also falls back to a
 * node-level GITHUB_TOKEN).
 */
export async function POST(req: NextRequest) {
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const team = String((body.team as string) || req.headers.get("x-hive-team") || "personal");
  const repoUrl = typeof body.repo_url === "string" ? body.repo_url : "";
  if (repoUrl.startsWith("https://github.com/")) {
    // Bounded so a slow/unreachable Composio never hangs the deploy — fall back to
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
  }
  // Forward the signed-in user's platform JWT so the backend's require_auth
  // accepts the deploy (a POST mutation) — without it, private-repo deploys
  // 401'd with "missing or invalid bearer token" even though the git_token for
  // the clone was attached. Platform auth (hive_jwt) and clone auth (git_token)
  // are two separate credentials; both are needed and never reach the browser.
  const r = await backend(
    "/v1/git/deploy",
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
