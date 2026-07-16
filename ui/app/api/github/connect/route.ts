import { NextRequest, NextResponse } from "next/server";
import { composioConfigured, githubConnect, resolveEntity } from "@/lib/composio";
import { authorizeUrl, currentUserId, githubAppConfigured, makeState, stateCookieName } from "@/lib/github-app";
import { publicOrigin } from "@/lib/origin";

export const dynamic = "force-dynamic";

export async function POST(req: NextRequest) {
  // Allow callers (e.g. the onboarding modal) to return to where they started.
  const body = await req.json().catch(() => ({} as any));
  const returnToRaw = typeof body?.returnTo === "string" ? body.returnTo : "/new";
  // Only accept same-origin relative paths to avoid open-redirects.
  const returnTo = returnToRaw.startsWith("/") ? returnToRaw : "/new";

  // FIRST-PARTY GitHub App (org-level permissions): preferred whenever configured.
  // No scopes param — capabilities come from the App's permissions + where it's
  // installed. The signed state carries returnTo; the nonce cookie binds it (CSRF).
  if (githubAppConfigured()) {
    // Bind the connection to the signed-in Clerk user (shared-browser safety):
    // the uid rides the SIGNED state through the OAuth dance into the cookie.
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

  // Legacy fallback: Composio-managed OAuth.
  if (!composioConfigured()) {
    return NextResponse.json(
      { error: "GitHub OAuth is not configured (set GITHUB_APP_CLIENT_ID/SECRET or COMPOSIO_API_KEY)." },
      { status: 400 }
    );
  }
  // Stable per-user entity (Clerk userId when signed in) so the connection made
  // here is the SAME one status/repos later read — no more random per-route ids.
  const entity = await resolveEntity();
  // Use the PUBLIC origin (honors ngrok/proxy headers) so GitHub redirects back
  // to the address the user is actually on, not the internal localhost.
  const origin = publicOrigin(req);
  const sep = returnTo.includes("?") ? "&" : "?";
  const redirectUrl = `${origin}${returnTo}${sep}connected=github`;
  const result = await githubConnect(entity, redirectUrl);
  return result.redirectUrl
    ? NextResponse.json({ redirectUrl: result.redirectUrl, provider: "composio" })
    : NextResponse.json({ error: result.error || "Failed to initiate GitHub connection" }, { status: 500 });
}
