import { NextResponse } from "next/server";
import { githubRepos, resolveEntity } from "@/lib/composio";

export const dynamic = "force-dynamic";

export async function GET() {
  // Same stable entity as status/connect — so a connected user's repos actually
  // resolve instead of coming back empty under a mismatched id.
  const entity = await resolveEntity();
  const repos = await githubRepos(entity);
  // Private to the browser, cached briefly so the page is instant on revisit.
  return NextResponse.json(
    { repos },
    { headers: { "Cache-Control": "private, max-age=300, stale-while-revalidate=600" } }
  );
}
