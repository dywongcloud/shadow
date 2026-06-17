import { NextRequest, NextResponse } from "next/server";
import { cookies } from "next/headers";
import { composioConfigured, githubConnect } from "@/lib/composio";

export const dynamic = "force-dynamic";

export async function POST(req: NextRequest) {
  if (!composioConfigured()) {
    return NextResponse.json(
      { error: "Composio not configured. Set COMPOSIO_API_KEY to enable GitHub OAuth." },
      { status: 400 }
    );
  }
  const c = cookies();
  let entity = c.get("hive_entity")?.value;
  if (!entity) entity = "user-" + Math.random().toString(36).slice(2, 10);
  const origin = req.nextUrl.origin;
  const redirectUrl = `${origin}/new?connected=github`;
  const url = await githubConnect(entity, redirectUrl);
  const res = url
    ? NextResponse.json({ redirectUrl: url })
    : NextResponse.json({ error: "Failed to initiate GitHub connection" }, { status: 500 });
  res.cookies.set("hive_entity", entity, { httpOnly: true, sameSite: "lax", path: "/" });
  return res;
}
