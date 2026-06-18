import { NextResponse } from "next/server";
import { cookies } from "next/headers";
import { githubRepos } from "@/lib/composio";

export const dynamic = "force-dynamic";

export async function GET() {
  const entity = cookies().get("hive_entity")?.value || "default";
  const repos = await githubRepos(entity);
  // Private to the browser, cached briefly so the page is instant on revisit.
  return NextResponse.json(
    { repos },
    { headers: { "Cache-Control": "private, max-age=300, stale-while-revalidate=600" } }
  );
}
