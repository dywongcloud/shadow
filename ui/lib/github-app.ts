import { createCipheriv, createDecipheriv, createHash, createHmac, randomBytes } from "crypto";
import { cookies } from "next/headers";

/**
 * First-party GitHub App OAuth (user-to-server) — the platform's OWN GitHub App
 * (org-level permissions), fixing private-repo/org access that Composio's managed
 * OAuth app couldn't grant. Flow:
 *
 *   /api/github/connect  → https://github.com/login/oauth/authorize (signed state)
 *   GitHub redirects to  → /oauth/github/callback (registered on the App)
 *   callback exchanges the code → user token stored ENCRYPTED in an httpOnly
 *   cookie (never readable by the browser, never persisted server-side), and all
 *   GitHub reads/writes/deploy-clones prefer this token via the lib/github facade.
 *
 * GitHub Apps do NOT use OAuth scopes — capabilities come from the App's
 * permissions plus which accounts/orgs it's INSTALLED on. Org access = the org
 * admin installs the app (see the install CTA in the Integrations card).
 */

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

// ---------------------------------------------------------------------------
// Cookie crypto — AES-256-GCM, key derived from the client secret. The token
// is never at rest in plaintext anywhere (browser sees only ciphertext in an
// httpOnly cookie; the server stores nothing).
// ---------------------------------------------------------------------------

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
    // Tampered/garbage/rotated-secret cookie → treated as disconnected, never a crash.
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
}

// ---------------------------------------------------------------------------
// State (CSRF) — HMAC-signed payload carried in the `state` param, bound to a
// nonce in a short-lived httpOnly cookie.
// ---------------------------------------------------------------------------

function sign(data: string): string {
  return createHmac("sha256", key()).update(data).digest("base64url");
}

export function makeState(returnTo: string): { state: string; nonce: string } {
  const nonce = randomBytes(16).toString("base64url");
  const body = Buffer.from(JSON.stringify({ n: nonce, r: returnTo, t: Date.now() })).toString("base64url");
  return { state: `${body}.${sign(body)}`, nonce };
}

export function verifyState(state: string, cookieNonce: string | undefined): { ok: boolean; returnTo: string } {
  try {
    const [body, mac] = state.split(".");
    if (!body || !mac || sign(body) !== mac) return { ok: false, returnTo: "/" };
    const j = JSON.parse(Buffer.from(body, "base64url").toString("utf8"));
    if (!cookieNonce || j.n !== cookieNonce) return { ok: false, returnTo: "/" };
    if (typeof j.t !== "number" || Date.now() - j.t > STATE_TTL_MS) return { ok: false, returnTo: "/" };
    const returnTo = typeof j.r === "string" && j.r.startsWith("/") ? j.r : "/";
    return { ok: true, returnTo };
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

// ---------------------------------------------------------------------------
// Token exchange / refresh — handles BOTH expiring (access+refresh) and
// non-expiring (access only) App configurations.
// ---------------------------------------------------------------------------

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

function bundleFrom(t: TokenResponse, prev?: GhAppTokenBundle): GhAppTokenBundle | null {
  if (!t.access_token) return null;
  const now = Date.now();
  return {
    at: t.access_token,
    rt: t.refresh_token || prev?.rt,
    // 60s safety margin so we refresh before the hard expiry.
    exp: t.expires_in ? now + (t.expires_in - 60) * 1000 : undefined,
    rexp: t.refresh_token_expires_in ? now + t.refresh_token_expires_in * 1000 : prev?.rexp,
    login: prev?.login,
    slug: prev?.slug,
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

// ---------------------------------------------------------------------------
// Request-scoped accessors (route handlers only — they may set cookies).
// ---------------------------------------------------------------------------

const COOKIE_OPTS = {
  httpOnly: true as const,
  secure: true as const,
  sameSite: "lax" as const,
  path: "/" as const,
  // Match the refresh token's 6-month lifetime; non-expiring tokens just live long.
  maxAge: 180 * 24 * 3600,
};

/** Persist a token bundle into the encrypted cookie (route-handler context). */
export async function setTokenCookie(b: GhAppTokenBundle): Promise<void> {
  (await cookies()).set(GH_APP_COOKIE, sealToken(b), COOKIE_OPTS);
}

export async function clearTokenCookie(): Promise<void> {
  (await cookies()).set(GH_APP_COOKIE, "", { ...COOKIE_OPTS, maxAge: 0 });
}

/** Read the current bundle (no refresh). */
export async function readTokenBundle(): Promise<GhAppTokenBundle | null> {
  const c = (await cookies()).get(GH_APP_COOKIE)?.value;
  return c ? openToken(c) : null;
}

/**
 * The live first-party access token for this request, auto-refreshing an expired
 * one (cookie updated in place — route-handler context required). Returns null
 * when not connected / refresh impossible, so callers fall back to Composio.
 */
export async function getGithubAppToken(): Promise<string | null> {
  const b = await readTokenBundle();
  if (!b) return null;
  if (!b.exp || Date.now() < b.exp) return b.at;
  // Expired → refresh (or give up and clear, so status honestly shows reconnect).
  const next = await refreshBundle(b).catch(() => null);
  if (next) {
    try {
      await setTokenCookie(next);
    } catch {
      /* outside a mutable-cookie context — still return the fresh token */
    }
    return next.at;
  }
  try {
    await clearTokenCookie();
  } catch {
    /* read-only context */
  }
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
