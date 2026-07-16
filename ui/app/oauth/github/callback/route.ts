import { NextRequest, NextResponse } from "next/server";
import {
  exchangeCode,
  githubAppConfigured,
  setTokenCookie,
  stateCookieName,
  verifyState,
} from "@/lib/github-app";

export const dynamic = "force-dynamic";

/**
 * OAuth callback for the platform's first-party GitHub App — the exact URL
 * registered on the App (https://shadw.cloud/oauth/github/callback). Verifies the
 * signed CSRF state, exchanges the code for a user-to-server token, seals it into
 * the encrypted httpOnly cookie, and returns the user to where they started with
 * `?connected=github` (the same contract the Composio flow used, so the
 * Integrations/GitOps/New-Project pages react identically).
 *
 * Every failure path redirects with a human-readable `github_error` flag — never
 * a raw 500, and never any credential material in a response.
 */
export async function GET(req: NextRequest) {
  const url = req.nextUrl;
  const back = (path: string, params: Record<string, string>) => {
    const u = new URL(path, url.origin);
    for (const [k, v] of Object.entries(params)) u.searchParams.set(k, v);
    const res = NextResponse.redirect(u);
    // One-shot state cookie is consumed either way.
    res.cookies.set(stateCookieName(), "", { httpOnly: true, path: "/", maxAge: 0 });
    return res;
  };

  if (!githubAppConfigured()) {
    return back("/integrations", { github_error: "GitHub App not configured on this deployment." });
  }

  // User declined the authorization on GitHub.
  const err = url.searchParams.get("error");
  if (err) {
    return back("/integrations", { github_error: err === "access_denied" ? "GitHub authorization was cancelled." : err });
  }

  const code = url.searchParams.get("code");
  const state = url.searchParams.get("state") || "";
  if (!code) {
    return back("/integrations", { github_error: "Missing authorization code." });
  }

  // CSRF: the state must be OUR signature and match the nonce cookie set at connect.
  const nonce = req.cookies.get(stateCookieName())?.value;
  const v = verifyState(state, nonce);
  if (!v.ok) {
    return back("/integrations", { github_error: "Invalid or expired sign-in state — please try connecting again." });
  }

  const bundle = await exchangeCode(code);
  if (!bundle) {
    return back(v.returnTo, { github_error: "GitHub token exchange failed — please try again." });
  }

  await setTokenCookie(bundle);
  const sep = v.returnTo.includes("?") ? "&" : "?";
  const res = NextResponse.redirect(new URL(`${v.returnTo}${sep}connected=github`, url.origin));
  res.cookies.set(stateCookieName(), "", { httpOnly: true, path: "/", maxAge: 0 });
  return res;
}
