import { NextRequest, NextResponse } from "next/server";
import {
  exchangeCode,
  githubAppConfigured,
  returnToFromSignedState,
  setTokenCookie,
  stateCookieName,
  verifyState,
} from "@/lib/github-app";
import { publicOrigin, oauthStateCookieDomain, secureCookie } from "@/lib/origin";
import { sameOriginPath } from "@/lib/same-origin-path";

/** OAuth callback for the first-party GitHub App (the URL registered on the App, https://shadw.cloud/oauth/github/callback): verifies the signed CSRF state, exchanges the
 *  code for a user token, seals it into the encrypted httpOnly cookie and returns to the page the user started on with `?connected=github`, the contract the Composio flow
 *  used. Every failure returns to that SAME page with a human-readable `github_error` that the pages render, never a raw 500 or credential material. */
export async function GET(req: NextRequest) {
  const url = req.nextUrl;
  const origin = publicOrigin(req);
  const domain = oauthStateCookieDomain(req.headers);
  const stateCookieOpts = {
    httpOnly: true,
    secure: secureCookie(req.headers),
    sameSite: "lax" as const,
    path: "/",
    ...(domain ? { domain } : {}),
  };
  const consumeStateCookie = (res: NextResponse) => {
    res.cookies.set(stateCookieName(), "", { ...stateCookieOpts, maxAge: 0 });
  };
  /** A failed connect must return to the page that started it, not teleport the user to
   *  /integrations: on a phone that reads like the connection silently vanished. */
  const startedOn = returnToFromSignedState(url.searchParams.get("state") || "") || "/integrations";
  const back = (path: string, params: Record<string, string>) => {
    const u = new URL(path, origin);
    for (const [k, v] of Object.entries(params)) u.searchParams.set(k, v);
    const res = NextResponse.redirect(u);
    consumeStateCookie(res);
    return res;
  };

  if (!githubAppConfigured()) {
    return back(startedOn, { github_error: "GitHub App not configured on this deployment." });
  }

  const providerError = url.searchParams.get("error");
  if (providerError) {
    return back(startedOn, { github_error: providerError === "access_denied" ? "GitHub authorization was cancelled." : providerError });
  }

  const code = url.searchParams.get("code");
  const state = url.searchParams.get("state") || "";
  if (!code) {
    return back(startedOn, { github_error: "Missing authorization code." });
  }

  const nonce = req.cookies.get(stateCookieName())?.value;
  const v = verifyState(state, nonce);
  if (!v.ok) {
    return back(startedOn, { github_error: "Invalid or expired sign-in state — please try connecting again." });
  }

  const returnTo = sameOriginPath(v.returnTo, "/");
  const exchange = await exchangeCode(code);
  if (exchange.kind !== "ok") {
    // A transient failure ("GitHub did not answer") must not read like a refusal, and neither one touches a connection the
    // browser already holds: the cookie is only ever written on success.
    return back(returnTo, {
      github_error:
        exchange.kind === "transient"
          ? "GitHub could not be reached to finish the connection — please try again in a moment."
          : "GitHub refused this authorization — please connect GitHub again.",
    });
  }
  const bundle = exchange.bundle;
  if (v.uid) bundle.uid = v.uid;
  await setTokenCookie(bundle);
  const sep = returnTo.includes("?") ? "&" : "?";
  const res = NextResponse.redirect(new URL(`${returnTo}${sep}connected=github`, origin));
  consumeStateCookie(res);
  return res;
}
