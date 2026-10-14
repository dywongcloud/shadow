import { NextRequest, NextResponse } from "next/server";
import { resolveEntity, githubAccessToken } from "@/lib/github";
import { authTokenFrom, backend } from "@/lib/gitops-server";

const GITHUB_TOKEN_LOOKUP_DEADLINE_MS = 8_000;

/** Deploy-from-Git entry point: the dashboard posts here instead of straight to the backend `/v1/git/deploy` so the signed-in user's connected-GitHub token can be attached
 *  SERVER-SIDE as `body.git_token` for a private-repo clone; it is never returned to the browser. Without a connection the clone is anonymous (the backend falls back to a
 *  node-level GITHUB_TOKEN). */
export async function POST(req: NextRequest) {
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const team = String((body.team as string) || req.headers.get("x-hive-team") || "personal");
  const repoUrl = typeof body.repo_url === "string" ? body.repo_url : "";
  if (repoUrl.startsWith("https://github.com/")) {
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
  }
  const r = await backend("/v1/git/deploy", team, authTokenFrom(req), { method: "POST", body: JSON.stringify(body) });
  const text = await r.text();
  return new NextResponse(text, {
    status: r.status,
    headers: { "content-type": r.headers.get("content-type") || "application/json" },
  });
}
