import { NextRequest, NextResponse } from "next/server";
import { composioConfigured, connectToolkit, resolveEntity } from "@/lib/composio";
import { publicOrigin } from "@/lib/origin";

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
  } catch {}
  if (!slug) {
    return NextResponse.json({ error: "Missing toolkit slug." }, { status: 400 });
  }

  const entity = await resolveEntity();
  const origin = publicOrigin(req);
  const redirectUrl = `${origin}/integrations?connected=${encodeURIComponent(slug)}`;
  const result = await connectToolkit(entity, slug, redirectUrl);
  return result.redirectUrl
    ? NextResponse.json({ redirectUrl: result.redirectUrl })
    : NextResponse.json({ error: result.error || "Failed to initiate connection" }, { status: 500 });
}
