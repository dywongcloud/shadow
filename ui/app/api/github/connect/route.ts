import { NextRequest, NextResponse } from "next/server";
import { composioConfigured, githubConnect, resolveEntity } from "@/lib/composio";
import { authorizeUrl, currentUserId, githubAppConfigured, makeState, stateCookieName } from "@/lib/github-app";
import { publicOrigin } from "@/lib/origin";

export async function POST(req: NextRequest) {
  const body = await req.json().catch(() => ({} as any));
  const returnToRaw = typeof body?.returnTo === "string" ? body.returnTo : "/new";
  const returnTo = returnToRaw.startsWith("/") ? returnToRaw : "/new";

  if (githubAppConfigured()) {
    const { state, nonce } = makeState(returnTo, await currentUserId());
    const res = NextResponse.json({ redirectUrl: authorizeUrl(state), provider: "github-app" });
    res.cookies.set(stateCookieName(), nonce, {
      httpOnly: true,
      secure: true,
      sameSite: "lax",
      path: "/",
      maxAge: 600,
    });
    return res;
  }

  if (!composioConfigured()) {
    return NextResponse.json(
      { error: "GitHub OAuth is not configured (set GITHUB_APP_CLIENT_ID/SECRET or COMPOSIO_API_KEY)." },
      { status: 400 }
    );
  }
  const entity = await resolveEntity();
  const origin = publicOrigin(req);
  const sep = returnTo.includes("?") ? "&" : "?";
  const redirectUrl = `${origin}${returnTo}${sep}connected=github`;
  const result = await githubConnect(entity, redirectUrl);
  if (result.redirectUrl) {
    return NextResponse.json({ redirectUrl: result.redirectUrl, provider: "composio" });
  }
  if (result.providerDisabled) {
    return NextResponse.json(
      {
        error: result.error,
        fallback: "manual-git-url",
        hint: "You can import any repository by its git URL from the New Project page — no GitHub connection required.",
      },
      { status: 503 }
    );
  }
  return NextResponse.json({ error: result.error || "Failed to initiate GitHub connection" }, { status: 500 });
}
