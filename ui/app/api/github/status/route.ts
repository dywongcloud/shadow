import { NextResponse } from "next/server";
import { cookies } from "next/headers";
import { composioConfigured, githubStatus } from "@/lib/composio";

export const dynamic = "force-dynamic";

export async function GET() {
  const c = cookies();
  let entity = c.get("hive_entity")?.value;
  const res = NextResponse.json({ configured: composioConfigured(), connected: false, entity: entity ?? null });
  if (!entity) {
    entity = "user-" + Math.random().toString(36).slice(2, 10);
    res.cookies.set("hive_entity", entity, { httpOnly: true, sameSite: "lax", path: "/" });
  }
  const status = await githubStatus(entity);
  return NextResponse.json({ ...status, entity }, { headers: res.headers });
}
