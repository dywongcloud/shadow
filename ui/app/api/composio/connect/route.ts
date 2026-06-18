import { NextRequest, NextResponse } from "next/server";
import { cookies } from "next/headers";
import { composioConfigured, connectToolkit } from "@/lib/composio";

export const dynamic = "force-dynamic";

export async function POST(req: NextRequest) {
  if (!composioConfigured()) {
    return NextResponse.json(
      { error: "Composio not configured. Set COMPOSIO_API_KEY to enable connecting integrations." },
      { status: 400 }
    );
  }
  let slug = "";
  try {
    const body = await req.json();
    slug = (body?.slug || "").toString().trim();
  } catch {
    // fall through to validation
  }
  if (!slug) {
    return NextResponse.json({ error: "Missing toolkit slug." }, { status: 400 });
  }

  const c = cookies();
  let entity = c.get("hive_entity")?.value;
  if (!entity) entity = "user-" + Math.random().toString(36).slice(2, 10);
  const origin = req.nextUrl.origin;
  const redirectUrl = `${origin}/integrations?connected=${encodeURIComponent(slug)}`;
  const result = await connectToolkit(entity, slug, redirectUrl);
  const res = result.redirectUrl
    ? NextResponse.json({ redirectUrl: result.redirectUrl })
    : NextResponse.json({ error: result.error || "Failed to initiate connection" }, { status: 500 });
  res.cookies.set("hive_entity", entity, { httpOnly: true, sameSite: "lax", path: "/" });
  return res;
}
