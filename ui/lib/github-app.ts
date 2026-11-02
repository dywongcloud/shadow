import { createCipheriv, createDecipheriv, createHash, createHmac, randomBytes } from "crypto";
import { auth } from "@clerk/nextjs/server";
import { cookies } from "next/headers";
import { sameOriginPath } from "@/lib/same-origin-path";

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
 *  and stores the user token ENCRYPTED in an httpOnly cookie (never readable by the browser, never persisted server-side);
 *  that cookie is the ONLY copy of the grant, so it is deleted only when GitHub itself refuses it — never because GitHub
 *  could not be reached, and never because a concurrent request already spent the single-use refresh token. GitHub reads,
 *  writes and deploy clones prefer it through the lib/github facade. An App has no OAuth scopes: capability = its permissions + where it is INSTALLED. */

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
    return { ok: true, returnTo: sameOriginPath(j.r, "/"), uid: typeof j.u === "string" ? j.u : undefined };
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
  message?: string;
}

const TOKEN_REQUEST_TIMEOUT_MS = 10_000;

/** Never throws: an UNANSWERED call (DNS/TLS/timeout/abort) comes back `status: 0`, which every caller must read as
 *  "no answer from GitHub", never as "GitHub said no". */
async function tokenRequest(params: Record<string, string>): Promise<{ status: number; body: TokenResponse }> {
  try {
    const r = await fetch(TOKEN_URL, {
      method: "POST",
      headers: { "content-type": "application/json", accept: "application/json" },
      body: JSON.stringify({ client_id: clientId(), client_secret: clientSecret(), ...params }),
      cache: "no-store",
      signal: AbortSignal.timeout(TOKEN_REQUEST_TIMEOUT_MS),
    });
    return { status: r.status, body: (await r.json().catch(() => ({}))) as TokenResponse };
  } catch {
    return { status: 0, body: {} };
  }
}

/** Why a GitHub call produced no credential. `definitive` = GitHub ANSWERED and refused it (revoked grant, invalid or
 *  already-spent refresh token, bad client credentials): retrying the same credential can never work, so the connection is
 *  genuinely gone. `transient` = the call got no answer, or an answer that says try again (5xx, 429, rate/abuse limits):
 *  the credential may be perfectly good. NOTHING is destroyed on the transient class — deleting the cookie here is what
 *  permanently disconnected a browser over one network blip, or over a single-use refresh token a second tab had spent. */
export type FailureClass = "definitive" | "transient";

/** GitHub's rate-limit/abuse refusals arrive as 403 or as a token-endpoint `error`, and both mean "later", not "never". */
const RETRYABLE_MESSAGE_RE = /rate limit|abuse detection|secondary rate|temporarily unavailable|server_error/i;

/** Classify an HTTP answer from GitHub. Status 0 (no answer at all) is always transient. */
export function classifyFailure(status: number, message?: string): FailureClass {
  if (status === 0) return "transient";
  if (status === 408 || status === 425 || status === 429) return "transient";
  if (status >= 500) return "transient";
  if (status === 403 && RETRYABLE_MESSAGE_RE.test(message || "")) return "transient";
  if (status >= 400 && status < 500) return "definitive";
  return "transient";
}

/** The token endpoint answers refusals in a 200 body (`error: "bad_refresh_token"`), so the body decides first. */
function classifyTokenFailure(status: number, body: TokenResponse): FailureClass {
  const msg = [body.error, body.error_description, body.message].filter(Boolean).join(" ");
  if (body.error) return RETRYABLE_MESSAGE_RE.test(msg) ? "transient" : "definitive";
  return classifyFailure(status, msg);
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

export type TokenExchangeResult =
  | { kind: "ok"; bundle: GhAppTokenBundle }
  | { kind: FailureClass };

export async function exchangeCode(code: string): Promise<TokenExchangeResult> {
  const { status, body } = await tokenRequest({ code, redirect_uri: githubAppRedirectUri() });
  const bundle = bundleFrom(body);
  if (bundle) return { kind: "ok", bundle };
  return { kind: classifyTokenFailure(status, body) };
}

export type RefreshOutcome =
  | { kind: "refreshed"; bundle: GhAppTokenBundle }
  /** The bundle carries no refresh token: with expiration off GitHub never sends one, and an expired token without one
   *  can never be replaced, so the grant is finished. */
  | { kind: "unrefreshable" }
  | { kind: FailureClass };

export async function refreshBundle(b: GhAppTokenBundle): Promise<RefreshOutcome> {
  if (!b.rt) return { kind: "unrefreshable" };
  const { status, body } = await tokenRequest({ grant_type: "refresh_token", refresh_token: b.rt });
  const next = bundleFrom(body, b);
  if (next) return { kind: "refreshed", bundle: next };
  return { kind: classifyTokenFailure(status, body) };
}

/** GitHub rotates refresh tokens SINGLE-USE, so two requests refreshing the same cookie race to spend the same token: the
 *  loser's refresh comes back refused, which used to delete the cookie and disconnect the browser for good. Refreshes are
 *  therefore single-flighted on the token's own digest: a request that arrives while one is in flight awaits ITS result
 *  (and gets the same rotated bundle) instead of spending the token a second time. The digest is the key, never the token.
 *  Bounded: one entry per in-flight refresh, deleted when it settles. This de-duplicates a race inside one server process;
 *  it cannot see a concurrent refresh in another worker or on another node, which is why a refusal must still be
 *  classified before anything is destroyed. */
const inflightRefreshes = new Map<string, Promise<RefreshOutcome>>();

function refreshKey(b: GhAppTokenBundle): string {
  return createHash("sha256").update(b.rt || "none").update(b.uid || "").digest("base64url").slice(0, 22);
}

async function refreshSingleFlight(b: GhAppTokenBundle): Promise<RefreshOutcome> {
  const k = refreshKey(b);
  const existing = inflightRefreshes.get(k);
  if (existing) return existing;
  const tracked = refreshBundle(b).finally(() => {
    if (inflightRefreshes.get(k) === tracked) inflightRefreshes.delete(k);
  });
  inflightRefreshes.set(k, tracked);
  return tracked;
}

const REFRESH_TOKEN_LIFETIME_SECS = 180 * 24 * 3600;

const COOKIE_OPTS = {
  httpOnly: true as const,
  secure: true as const,
  sameSite: "lax" as const,
  path: "/" as const,
  maxAge: REFRESH_TOKEN_LIFETIME_SECS,
};

/** Deliberately NO `domain`: a host-only cookie. Setting one would widen the credential to every subdomain of the
 *  dashboard's parent domain (any XSS or any app on a sibling host would then be able to send it), so sharing this cookie
 *  across the shadw.cloud round-robin hosts is NOT done by widening it — that is a security change and needs the
 *  operator's word. The other half of that problem (a host whose GITHUB_APP_CLIENT_SECRET differs cannot decrypt a cookie
 *  another host minted) is a deployment-parity question, not a cookie-attribute one; `credential: "unreadable"` reports it
 *  instead of silently reading as "not connected". */

/** Persist a token bundle into the encrypted cookie (route-handler context). */
export async function setTokenCookie(b: GhAppTokenBundle): Promise<void> {
  (await cookies()).set(GH_APP_COOKIE, sealToken(b), COOKIE_OPTS);
}

export async function clearTokenCookie(): Promise<void> {
  (await cookies()).set(GH_APP_COOKIE, "", { ...COOKIE_OPTS, maxAge: 0 });
}

/** How much of a GitHub connection THIS request can see. `none` = no cookie; `foreign` = a cookie that belongs to another
 *  signed-in user; `unreadable` = a cookie is present but cannot be decrypted here (a host whose client secret differs, or a
 *  rotated key) — that is a deployment-parity fault, and reporting it beats reading it as "not connected". */
export type BundleReadState = "ok" | "none" | "foreign" | "unreadable";

export async function readTokenBundleResult(): Promise<{ bundle: GhAppTokenBundle | null; state: BundleReadState }> {
  const raw = (await cookies()).get(GH_APP_COOKIE)?.value;
  if (!raw) return { bundle: null, state: "none" };
  const b = openToken(raw);
  if (!b) return { bundle: null, state: "unreadable" };
  if (!(await bundleAuthorized(b))) return { bundle: null, state: "foreign" };
  return { bundle: b, state: "ok" };
}

/** Reads the current bundle (no refresh), only when it belongs to the current Clerk user: foreign or unbound bundles read as disconnected. */
export async function readTokenBundle(): Promise<GhAppTokenBundle | null> {
  return (await readTokenBundleResult()).bundle;
}

/** What we hold for this browser, and whether GitHub was actually reached. `valid` = a usable access token;
 *  `refresh-unavailable` = we still HOLD the grant (cookie untouched) but GitHub could not be reached to rotate it, so the
 *  caller must degrade to the last known good state, never to "disconnected"; `rejected` = GitHub itself refused the
 *  grant and the cookie has been cleared. */
export type CredentialState =
  | "none"
  | "foreign"
  | "unreadable"
  | "valid"
  | "refresh-unavailable"
  | "rejected";

export interface GithubCredential {
  /** A token GitHub is expected to accept, or null when none is available right now. */
  token: string | null;
  state: CredentialState;
  /** The stored bundle when one is held: its cached `login` is the last known good identity, and is what a caller shows
   *  while GitHub is unreachable. Never a token for a request that cannot use one. */
  bundle: GhAppTokenBundle | null;
}

/** The credential this request holds, with WHY it holds no token when it does not. */
export async function getGithubAppTokenResult(): Promise<GithubCredential> {
  const { bundle, state } = await readTokenBundleResult();
  if (!bundle) return { token: null, state: state === "ok" ? "none" : state, bundle: null };
  if (!bundle.exp || Date.now() < bundle.exp) return { token: bundle.at, state: "valid", bundle };

  const r = await refreshSingleFlight(bundle);
  if (r.kind === "refreshed") {
    try {
      await setTokenCookie(r.bundle);
    } catch {}
    return { token: r.bundle.at, state: "valid", bundle: r.bundle };
  }
  if (r.kind === "transient") {
    // GitHub did not answer. The cookie is the ONLY copy of this grant: keep it, and let the caller degrade.
    return { token: null, state: "refresh-unavailable", bundle };
  }
  // "unrefreshable" (expired with nothing to rotate) and "definitive" (GitHub refused the grant) are both final.
  try {
    await clearTokenCookie();
  } catch {}
  return { token: null, state: "rejected", bundle: null };
}

/** The live first-party access token for this request, refreshing an expired one (cookie updated in place); null when not connected or the refresh is
 *  impossible, so callers fall back to Composio. Call ONLY from a Route Handler (a mutable-cookie context): GitHub rotates refresh tokens SINGLE-USE, so
 *  a refresh from a Server Component or middleware would burn the old token and persist nothing, and every later refresh would fail like a revoked App. */
export async function getGithubAppToken(): Promise<string | null> {
  return (await getGithubAppTokenResult()).token;
}

/** Did GitHub ANSWER about this token? `rejected` is the only class that says the credential is really dead: 401/403 means
 *  revoked or stale, while no answer at all (DNS, TLS, timeout, 5xx, rate limit) says nothing about it. Used to keep a live
 *  probe from being read as a verdict on a credential that just could not be tested. */
export type ProbeOutcome =
  | { kind: "ok"; login: string }
  | { kind: "unreachable" }
  | { kind: "rejected" };

const PROBE_TIMEOUT_MS = 8_000;

export async function probeGithubUser(token: string): Promise<ProbeOutcome> {
  let status = 0;
  let message = "";
  try {
    const r = await fetch("https://api.github.com/user", {
      headers: {
        authorization: `Bearer ${token}`,
        accept: "application/vnd.github+json",
        "x-github-api-version": "2022-11-28",
        "user-agent": "shadw-cloud",
      },
      cache: "no-store",
      signal: AbortSignal.timeout(PROBE_TIMEOUT_MS),
    });
    status = r.status;
    const j = (await r.json().catch(() => ({}))) as { login?: string; message?: string };
    if (status >= 200 && status < 300 && j.login) return { kind: "ok", login: j.login };
    message = j.message || "";
  } catch {
    return { kind: "unreachable" };
  }
  return classifyFailure(status, message) === "transient" ? { kind: "unreachable" } : { kind: "rejected" };
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
