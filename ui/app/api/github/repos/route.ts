import { NextResponse } from "next/server";
import { cookies } from "next/headers";
import { githubRepos } from "@/lib/composio";

export const dynamic = "force-dynamic";

export async function GET() {
  const entity = cookies().get("hive_entity")?.value || "default";
  const repos = await githubRepos(entity);
  return NextResponse.json({ repos });
}
