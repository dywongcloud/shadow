import "server-only";

const APEX = (process.env.NEXT_PUBLIC_HIVE_PLATFORM_DOMAIN || "shadw.cloud").toLowerCase();

/** `NextRequest.headers`, `ReadonlyHeaders` and `Headers` all satisfy this. */
export type HeaderLike = { get(name: string): string | null };

function firstValue(v: string | null): string {
  return v ? v.split(",")[0].trim() : "";
}

/** The app's PUBLIC origin for this request, the base for OAuth callback URLs: rebuilt from `x-forwarded-host`/`host` and `x-forwarded-proto` so it is the address the user
 *  actually came in on (behind a proxy, tunnel or load balancer), not the internal `http://localhost`. */
export function publicOrigin(req: { headers: HeaderLike; nextUrl?: { host: string; protocol: string } }): string {
  const host =
    firstValue(req.headers.get("x-forwarded-host")) ||
    firstValue(req.headers.get("host")) ||
    req.nextUrl?.host ||
    "";
  const proto =
    firstValue(req.headers.get("x-forwarded-proto")) ||
    req.nextUrl?.protocol.replace(":", "") ||
    "https";
  return `${proto}://${host}`;
}

/** Parent-domain scope for the short-lived OAuth state nonce, so a flow started on `www.<apex>`
 *  can be completed on the registered apex callback (or vice versa). A host-only nonce is lost on
 *  that hop and the callback reports "Invalid or expired sign-in state". This deliberately does
 *  NOT apply to `gh_app_tok`: widening the credential itself is a separate security decision.
 *  A sibling host can overwrite this nonce, but cannot forge the HMAC-signed `state`, and the
 *  state carries the signing user's Clerk uid: a login-CSRF bundle then fails `bundleAuthorized`
 *  as `foreign`. Only our own apex or one of its subdomains is eligible; anything else stays
 *  host-only. */
export function oauthStateCookieDomain(headers: HeaderLike): string | undefined {
  const origin = publicOrigin({ headers });
  let host: string;
  try {
    host = new URL(origin).hostname.toLowerCase();
  } catch {
    return undefined;
  }
  if (host !== APEX && !host.endsWith(`.${APEX}`)) return undefined;
  return `.${APEX}`;
}

/** `Secure` is correct on https and fatal on an http LAN preview: the browser drops the cookie
 *  and the whole connect flow silently restarts. Mirrors `hive_jwt`'s production-only `Secure`. */
export function secureCookie(headers: HeaderLike): boolean {
  const proto = firstValue(headers.get("x-forwarded-proto"));
  if (proto) return proto === "https";
  const host = firstValue(headers.get("x-forwarded-host")) || firstValue(headers.get("host"));
  if (/^(localhost|127\.0\.0\.1|\[::1\])(:|$)/i.test(host)) return false;
  return process.env.NODE_ENV === "production";
}
