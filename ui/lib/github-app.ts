import { createCipheriv, createDecipheriv, createHash, createHmac, randomBytes } from "crypto";
import { auth } from "@clerk/nextjs/server";
import { cookies } from "next/headers";

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY || !!process.env.CLERK_SECRET_KEY;

/** Current Clerk user id, or null (signed-out / local no-auth mode / non-request ctx). */
export async function currentUserId(): Promise<string | null> {
  if (!clerkEnabled) return null;
  try {
    const { userId } = await auth();
    return userId || null;
  } catch {
    return null;
  }
}

/** First-party GitHub App OAuth (user-to-server): the platform's OWN GitHub App (org-level permissions), fixing the private-repo/org access
 *  Composio's managed app could not grant. `/api/github/connect` -> GitHub authorize (signed state) -> `/oauth/github/callback` exchanges the code
 *  and stores the user token ENCRYPTED in an httpOnly cookie (never readable by the browser, never persisted server-side); GitHub reads, writes
 *  and deploy clones prefer it through the lib/github facade. An App has no OAuth scopes: capability = its permissions + where it is INSTALLED. */

const AUTHORIZE = "https://github.com/login/oauth/authorize";
const TOKEN_URL = "https://github.com/login/oauth/access_token";

export const GH_APP_COOKIE = "gh_app_tok";
const STATE_COOKIE = "gh_oauth_state";
const STATE_TTL_MS = 10 * 60_000;

export function githubAppConfigured(): boolean {
  return !!process.env.GITHUB_APP_CLIENT_ID && !!process.env.GITHUB_APP_CLIENT_SECRET;
}

function clientId(): string {
  return process.env.GITHUB_APP_CLIENT_ID || "";
}
function clientSecret(): string {
  return process.env.GITHUB_APP_CLIENT_SECRET || "";
}

/** The exact redirect URI registered on the GitHub App. */
export function githubAppRedirectUri(): string {
  return process.env.GITHUB_APP_REDIRECT_URI || "https://shadw.cloud/oauth/github/callback";
}

/** AES-256-GCM key derived from the App's client secret: the token is never at rest in plaintext (the browser holds only ciphertext in an httpOnly
 *  cookie, the server nothing). */

function key(): Buffer {
  return createHash("sha256").update(`gh-app-cookie:${clientSecret()}`).digest();
}

export function sealToken(payload: object): string {
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key(), iv);
  const ct = Buffer.concat([cipher.update(JSON.stringify(payload), "utf8"), cipher.final()]);
  return Buffer.concat([iv, cipher.getAuthTag(), ct]).toString("base64url");
}

export function openToken(sealed: string): GhAppTokenBundle | null {
  try {
    const raw = Buffer.from(sealed, "base64url");
    const iv = raw.subarray(0, 12);
    const tag = raw.subarray(12, 28);
    const ct = raw.subarray(28);
    const decipher = createDecipheriv("aes-256-gcm", key(), iv);
    decipher.setAuthTag(tag);
    const pt = Buffer.concat([decipher.update(ct), decipher.final()]).toString("utf8");
    const j = JSON.parse(pt);
    return typeof j?.at === "string" && j.at ? (j as GhAppTokenBundle) : null;
  } catch {
    return null;
  }
}

export interface GhAppTokenBundle {
  /** access token (ghu_…) */
  at: string;
  /** refresh token (ghr_…) — absent when the App has token expiration disabled */
  rt?: string;
  /** access-token expiry, ms epoch — absent = non-expiring */
  exp?: number;
  /** refresh-token expiry, ms epoch */
  rexp?: number;
  /** cached GitHub login for display */
  login?: string;
  /** cached app slug (from /user/installations) for the install CTA */
  slug?: string;
  /** Clerk userId the connection belongs to: a different signed-in user (or a signed-out request) never sees or disconnects another user's
   *  connection from a shared browser; absent in local no-Clerk mode. */
  uid?: string;
}

/** True when the bundle belongs to the CURRENT Clerk user (or Clerk is off). */
async function bundleAuthorized(b: GhAppTokenBundle): Promise<boolean> {
  if (!clerkEnabled) return true;
  if (!b.uid) return false;
  return (await currentUserId()) === b.uid;
}

function sign(data: string): string {
  return createHmac("sha256", key()).update(data).digest("base64url");
}

export function makeState(returnTo: string, uid?: string | null): { state: string; nonce: string } {
  const nonce = randomBytes(16).toString("base64url");
  const body = Buffer.from(
    JSON.stringify({ n: nonce, r: returnTo, t: Date.now(), ...(uid ? { u: uid } : {}) })
  ).toString("base64url");
  return { state: `${body}.${sign(body)}`, nonce };
}

export function verifyState(
  state: string,
  cookieNonce: string | undefined
): { ok: boolean; returnTo: string; uid?: string } {
  try {
    const [body, mac] = state.split(".");
    if (!body || !mac || sign(body) !== mac) return { ok: false, returnTo: "/" };
    const j = JSON.parse(Buffer.from(body, "base64url").toString("utf8"));
    if (!cookieNonce || j.n !== cookieNonce) return { ok: false, returnTo: "/" };
    if (typeof j.t !== "number" || Date.now() - j.t > STATE_TTL_MS) return { ok: false, returnTo: "/" };
    const returnTo = typeof j.r === "string" && j.r.startsWith("/") ? j.r : "/";
    return { ok: true, returnTo, uid: typeof j.u === "string" ? j.u : undefined };
  } catch {
    return { ok: false, returnTo: "/" };
  }
}

export function stateCookieName(): string {
  return STATE_COOKIE;
}

/** Build the GitHub authorize URL (no scopes — GitHub Apps use installation permissions). */
export function authorizeUrl(state: string): string {
  const u = new URL(AUTHORIZE);
  u.searchParams.set("client_id", clientId());
  u.searchParams.set("redirect_uri", githubAppRedirectUri());
  u.searchParams.set("state", state);
  return u.toString();
}

interface TokenResponse {
  access_token?: string;
  expires_in?: number;
  refresh_token?: string;
  refresh_token_expires_in?: number;
  error?: string;
  error_description?: string;
}

async function tokenRequest(params: Record<string, string>): Promise<TokenResponse> {
  const r = await fetch(TOKEN_URL, {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify({ client_id: clientId(), client_secret: clientSecret(), ...params }),
    cache: "no-store",
  });
  return (await r.json().catch(() => ({}))) as TokenResponse;
}

const EXPIRY_SAFETY_MARGIN_SECS = 60;

function bundleFrom(t: TokenResponse, prev?: GhAppTokenBundle): GhAppTokenBundle | null {
  if (!t.access_token) return null;
  const now = Date.now();
  return {
    at: t.access_token,
    rt: t.refresh_token || prev?.rt,
    exp: t.expires_in ? now + (t.expires_in - EXPIRY_SAFETY_MARGIN_SECS) * 1000 : undefined,
    rexp: t.refresh_token_expires_in ? now + t.refresh_token_expires_in * 1000 : prev?.rexp,
    login: prev?.login,
    slug: prev?.slug,
    uid: prev?.uid,
  };
}

export async function exchangeCode(code: string): Promise<GhAppTokenBundle | null> {
  const t = await tokenRequest({ code, redirect_uri: githubAppRedirectUri() });
  return bundleFrom(t);
}

export async function refreshBundle(b: GhAppTokenBundle): Promise<GhAppTokenBundle | null> {
  if (!b.rt) return null;
  const t = await tokenRequest({ grant_type: "refresh_token", refresh_token: b.rt });
  return bundleFrom(t, b);
}

const REFRESH_TOKEN_LIFETIME_SECS = 180 * 24 * 3600;

const COOKIE_OPTS = {
  httpOnly: true as const,
  secure: true as const,
  sameSite: "lax" as const,
  path: "/" as const,
  maxAge: REFRESH_TOKEN_LIFETIME_SECS,
};

/** Persist a token bundle into the encrypted cookie (route-handler context). */
export async function setTokenCookie(b: GhAppTokenBundle): Promise<void> {
  (await cookies()).set(GH_APP_COOKIE, sealToken(b), COOKIE_OPTS);
}

export async function clearTokenCookie(): Promise<void> {
  (await cookies()).set(GH_APP_COOKIE, "", { ...COOKIE_OPTS, maxAge: 0 });
}

/** Reads the current bundle (no refresh), only when it belongs to the current Clerk user: foreign or unbound bundles read as disconnected. */
export async function readTokenBundle(): Promise<GhAppTokenBundle | null> {
  const c = (await cookies()).get(GH_APP_COOKIE)?.value;
  const b = c ? openToken(c) : null;
  if (!b) return null;
  return (await bundleAuthorized(b)) ? b : null;
}

/** The live first-party access token for this request, refreshing an expired one (cookie updated in place); null when not connected or the refresh is
 *  impossible, so callers fall back to Composio. Call ONLY from a Route Handler (a mutable-cookie context): GitHub rotates refresh tokens SINGLE-USE, so
 *  a refresh from a Server Component or middleware would burn the old token and persist nothing, and every later refresh would fail like a revoked App. */
export async function getGithubAppToken(): Promise<string | null> {
  const b = await readTokenBundle();
  if (!b) return null;
  if (!b.exp || Date.now() < b.exp) return b.at;
  const next = await refreshBundle(b).catch(() => null);
  if (next) {
    try {
      await setTokenCookie(next);
    } catch {}
    return next.at;
  }
  try {
    await clearTokenCookie();
  } catch {}
  return null;
}

/** Update cached display fields (login/app slug) on the bundle. */
export async function updateBundleMeta(meta: { login?: string; slug?: string }): Promise<void> {
  const b = await readTokenBundle();
  if (!b) return;
  await setTokenCookie({ ...b, ...meta });
}

/** Best-effort revoke of the user's authorization on GitHub (used by disconnect). */
export async function revokeAppGrant(accessToken: string): Promise<boolean> {
  try {
    const basic = Buffer.from(`${clientId()}:${clientSecret()}`).toString("base64");
    const r = await fetch(`https://api.github.com/applications/${encodeURIComponent(clientId())}/grant`, {
      method: "DELETE",
      headers: {
        authorization: `Basic ${basic}`,
        accept: "application/vnd.github+json",
        "content-type": "application/json",
      },
      body: JSON.stringify({ access_token: accessToken }),
      cache: "no-store",
    });
    return r.status === 204;
  } catch {
    return false;
  }
}
