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
  // Allow callers (e.g. the onboarding modal) to return to where they started.
  const body = await req.json().catch(() => ({} as any));
  const returnToRaw = typeof body?.returnTo === "string" ? body.returnTo : "/new";
  // Only accept same-origin relative paths to avoid open-redirects.
  const returnTo = returnToRaw.startsWith("/") ? returnToRaw : "/new";
  const sep = returnTo.includes("?") ? "&" : "?";
  const redirectUrl = `${origin}${returnTo}${sep}connected=github`;
  const result = await githubConnect(entity, redirectUrl);
  const res = result.redirectUrl
    ? NextResponse.json({ redirectUrl: result.redirectUrl })
    : NextResponse.json({ error: result.error || "Failed to initiate GitHub connection" }, { status: 500 });
  res.cookies.set("hive_entity", entity, { httpOnly: true, sameSite: "lax", path: "/" });
  return res;
}
