import { NextRequest, NextResponse } from "next/server";
import {
  exchangeCode,
  githubAppConfigured,
  setTokenCookie,
  stateCookieName,
  verifyState,
} from "@/lib/github-app";
import { publicOrigin } from "@/lib/origin";

/** OAuth callback for the first-party GitHub App (the URL registered on the App, https://shadw.cloud/oauth/github/callback): verifies the signed CSRF state, exchanges the
 *  code for a user token, seals it into the encrypted httpOnly cookie and returns to the page the user started on with `?connected=github`, the contract the Composio flow
 *  used. Every failure redirects with a human-readable `github_error`, never a raw 500 or credential material. */
export async function GET(req: NextRequest) {
  const url = req.nextUrl;
  const origin = publicOrigin(req);
  const consumeStateCookie = (res: NextResponse) => {
    res.cookies.set(stateCookieName(), "", { httpOnly: true, path: "/", maxAge: 0 });
  };
  const back = (path: string, params: Record<string, string>) => {
    const u = new URL(path, origin);
    for (const [k, v] of Object.entries(params)) u.searchParams.set(k, v);
    const res = NextResponse.redirect(u);
    consumeStateCookie(res);
    return res;
  };

  if (!githubAppConfigured()) {
    return back("/integrations", { github_error: "GitHub App not configured on this deployment." });
  }

  const providerError = url.searchParams.get("error");
  if (providerError) {
    return back("/integrations", { github_error: providerError === "access_denied" ? "GitHub authorization was cancelled." : providerError });
  }

  const code = url.searchParams.get("code");
  const state = url.searchParams.get("state") || "";
  if (!code) {
    return back("/integrations", { github_error: "Missing authorization code." });
  }

  const nonce = req.cookies.get(stateCookieName())?.value;
  const v = verifyState(state, nonce);
  if (!v.ok) {
    return back("/integrations", { github_error: "Invalid or expired sign-in state — please try connecting again." });
  }

  const bundle = await exchangeCode(code);
  if (!bundle) {
    return back(v.returnTo, { github_error: "GitHub token exchange failed — please try again." });
  }

  if (v.uid) bundle.uid = v.uid;
  await setTokenCookie(bundle);
  const sep = v.returnTo.includes("?") ? "&" : "?";
  const res = NextResponse.redirect(new URL(`${v.returnTo}${sep}connected=github`, origin));
  consumeStateCookie(res);
  return res;
}
