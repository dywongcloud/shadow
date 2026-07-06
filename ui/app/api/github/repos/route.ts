import { NextRequest, NextResponse } from "next/server";
import { githubRepos, githubOrgRepos, resolveEntity } from "@/lib/composio";

export const dynamic = "force-dynamic";

export async function GET(req: NextRequest) {
  // Same stable entity as status/connect — so a connected user's repos actually
  // resolve instead of coming back empty under a mismatched id.
  const entity = await resolveEntity();
  // `?org=<login>` → list THAT organization's repos (GitOps org scope); otherwise
  // the authenticated user's personal repos. Fixes the picker showing personal
  // repos when the user selected an organization scope.
  const org = req.nextUrl.searchParams.get("org")?.trim();
  const repos = org ? await githubOrgRepos(entity, org) : await githubRepos(entity);
  // Private to the browser, cached briefly so the page is instant on revisit.
  return NextResponse.json(
    { repos },
    { headers: { "Cache-Control": "private, max-age=300, stale-while-revalidate=600" } }
  );
}
