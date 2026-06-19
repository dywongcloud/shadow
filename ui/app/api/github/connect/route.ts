import { NextRequest, NextResponse } from "next/server";
import { composioConfigured, githubConnect, resolveEntity } from "@/lib/composio";
import { publicOrigin } from "@/lib/origin";

export const dynamic = "force-dynamic";

export async function POST(req: NextRequest) {
  if (!composioConfigured()) {
    return NextResponse.json(
      { error: "Composio not configured. Set COMPOSIO_API_KEY to enable GitHub OAuth." },
      { status: 400 }
    );
  }
  // Stable per-user entity (Clerk userId when signed in) so the connection made
  // here is the SAME one status/repos later read — no more random per-route ids.
  const entity = await resolveEntity();
  // Use the PUBLIC origin (honors ngrok/proxy headers) so GitHub redirects back
  // to the address the user is actually on (e.g. https://shadow.ngrok.pizza),
  // not the internal http://localhost the server sees.
  const origin = publicOrigin(req);
  // Allow callers (e.g. the onboarding modal) to return to where they started.
  const body = await req.json().catch(() => ({} as any));
  const returnToRaw = typeof body?.returnTo === "string" ? body.returnTo : "/new";
  // Only accept same-origin relative paths to avoid open-redirects.
  const returnTo = returnToRaw.startsWith("/") ? returnToRaw : "/new";
  const sep = returnTo.includes("?") ? "&" : "?";
  const redirectUrl = `${origin}${returnTo}${sep}connected=github`;
  const result = await githubConnect(entity, redirectUrl);
  return result.redirectUrl
    ? NextResponse.json({ redirectUrl: result.redirectUrl })
    : NextResponse.json({ error: result.error || "Failed to initiate GitHub connection" }, { status: 500 });
}
