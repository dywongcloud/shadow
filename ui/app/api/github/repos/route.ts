import { NextRequest, NextResponse } from "next/server";
import { githubRepos, githubOrgRepos, resolveEntity } from "@/lib/github";

const REPOS_CACHE_CONTROL = "private, max-age=300, stale-while-revalidate=600";

/** The connected user's repositories, or with `?org=<login>` THAT organization's (GitOps org scope) plus the restriction signal (`restricted`, `approve_url`) so the picker can offer an
 *  "approve this app for the organization" link instead of an unexplained empty list. */
export async function GET(req: NextRequest) {
  const entity = await resolveEntity();
  const org = req.nextUrl.searchParams.get("org")?.trim();
  if (org) {
    const r = await githubOrgRepos(entity, org);
    return NextResponse.json(
      { repos: r.repos, restricted: r.restricted, approve_url: r.approve_url },
      { headers: { "Cache-Control": REPOS_CACHE_CONTROL } }
    );
  }
  const repos = await githubRepos(entity);
  return NextResponse.json(
    { repos },
    { headers: { "Cache-Control": REPOS_CACHE_CONTROL } }
  );
}
