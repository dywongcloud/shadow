// GitHub App SERVER-TO-SERVER auth for the dashboard — the App's own identity
// (RS256 JWT from GITHUB_APP_ID + GITHUB_APP_PRIVATE_KEY), NOT the per-browser
// user-to-server OAuth cookie (./github-app) and NOT Composio (./composio).
//
// This is the dashboard's half of the SAME credential + flow the hive-cloud
// node's webhook/token-resolution path uses
// (crates/hive-cloud/src/github_app_auth.rs — same env var names, same claim
// shape, same two GitHub endpoints). The node uses it to clone private repos
// on a webhook delivery with no user session; the dashboard uses it for the
// two things the per-browser cookie could never be the source of truth for:
//
//   1. CONNECT STATE — `GET /app/installations` answers "is the GitHub App
//      installed on any account" from GitHub's own record, so a fresh browser
//      (no gh_app_tok cookie) still shows the real connection state instead of
//      wrongly prompting to connect. The cookie degrades to a cache.
//   2. OUTBOUND WRITES — `POST /app/installations/{id}/access_tokens` mints a
//      short-lived installation token so the GitOps writers (config-repo sync,
//      project CI webhook/workflow install) actually apply even when the
//      triggering request carries no user token, instead of silently no-op'ing
//      on "github-not-connected".
//
// SECURITY (mirrors the Rust module): never log or return the private key, a
// signed JWT, or a minted installation token — errors carry GitHub's HTTP
// status + message only (GitHub error bodies never echo credentials back).
import "server-only";
import { createSign } from "crypto";

const API_BASE = "https://api.github.com";
const API_VERSION = "2022-11-28";
const USER_AGENT = "shadw-cloud";
const TIMEOUT_MS = 10_000;
// GitHub caps App JWTs at 10 minutes; stay comfortably under. `iat` is
// backdated 60s to tolerate clock drift (GitHub's own recommendation, and the
// same values the node's github_app_auth uses).
const JWT_BACKDATE_SECS = 60;
const JWT_TTL_SECS = 9 * 60;

function appId(): string {
  return process.env.GITHUB_APP_ID || "";
}

/** The PEM, accepting the single-line `\n`-escaped form ops tooling produces
 *  (systemd Environment= / ansible-vault scalar), same as the node module. */
function privateKeyPem(): string {
  const raw = process.env.GITHUB_APP_PRIVATE_KEY || "";
  return raw.includes("\\n") && !raw.includes("\n") ? raw.replace(/\\n/g, "\n") : raw;
}

/** True when the App's server-to-server identity is configured (both parts). */
export function installationAuthConfigured(): boolean {
  return !!appId() && !!privateKeyPem();
}

/** Sign a fresh App JWT (RS256, ~9 min valid). Throws on a malformed key —
 *  callers wrap it into their typed-error result. */
function signAppJwt(): string {
  const now = Math.floor(Date.now() / 1000);
  const b64 = (o: object) => Buffer.from(JSON.stringify(o), "utf8").toString("base64url");
  const body = `${b64({ alg: "RS256", typ: "JWT" })}.${b64({
    iat: now - JWT_BACKDATE_SECS,
    exp: now + JWT_TTL_SECS,
    iss: appId(),
  })}`;
  const sig = createSign("RSA-SHA256").update(body).sign(privateKeyPem(), "base64url");
  return `${body}.${sig}`;
}

/** One authenticated call against the GitHub API with the App JWT. NEVER
 *  throws — network failure surfaces as status 0 so callers get one uniform
 *  typed-error path. */
async function appFetch(
  jwt: string,
  path: string,
  init?: RequestInit
): Promise<{ status: number; json: any }> {
  try {
    const r = await fetch(path.startsWith("http") ? path : `${API_BASE}${path}`, {
      ...init,
      headers: {
        Authorization: `Bearer ${jwt}`,
        Accept: "application/vnd.github+json",
        "X-GitHub-Api-Version": API_VERSION,
        "User-Agent": USER_AGENT,
        "Content-Type": "application/json",
        ...(init?.headers || {}),
      },
      cache: "no-store",
      signal: AbortSignal.timeout(TIMEOUT_MS),
    });
    const text = await r.text();
    let json: any = {};
    try {
      json = text ? JSON.parse(text) : {};
    } catch {
      json = { raw: text };
    }
    return { status: r.status, json };
  } catch (e: any) {
    return { status: 0, json: { message: e?.message || "network error" } };
  }
}

function errOf(status: number, json: any): string {
  const msg = typeof json?.message === "string" ? json.message : "";
  return msg ? `HTTP ${status}: ${msg}` : `HTTP ${status || "network-error"}`;
}

export interface AppInstallation {
  id: number;
  app_slug?: string;
  account_login?: string;
  account_type?: string;
}

export type InstallationsResult =
  | { ok: true; installations: AppInstallation[] }
  | { ok: false; error: string };

/**
 * The App's REAL installation record (`GET /app/installations`) — every
 * account (user or org) this GitHub App is installed on, per GitHub itself.
 * This is the connect-state source of truth the per-browser cookie replaced:
 * an installation here is valid regardless of any browser's cookie state.
 * An empty list is a REAL "not installed anywhere" answer; `{ok:false}` means
 * the question couldn't be asked (bad/absent key, GitHub down) and callers
 * must treat the state as UNKNOWN, never as disconnected-or-connected.
 */
export async function listAppInstallations(): Promise<InstallationsResult> {
  if (!installationAuthConfigured()) {
    return { ok: false, error: "GITHUB_APP_ID/GITHUB_APP_PRIVATE_KEY not set" };
  }
  let jwt: string;
  try {
    jwt = signAppJwt();
  } catch (e: any) {
    return { ok: false, error: `GITHUB_APP_PRIVATE_KEY is not a valid RSA PEM: ${e?.message || e}` };
  }
  const { status, json } = await appFetch(jwt, "/app/installations?per_page=100");
  if (status < 200 || status >= 300) return { ok: false, error: errOf(status, json) };
  const arr: any[] = Array.isArray(json) ? json : [];
  return {
    ok: true,
    installations: arr
      .filter((i: any) => typeof i?.id === "number")
      .map((i: any) => ({
        id: i.id,
        app_slug: i.app_slug,
        account_login: i.account?.login,
        account_type: i.account?.type,
      })),
  };
}

export type InstallationTokenResult =
  | { ok: true; /** null = the App simply isn't installed on this repo (not an error). */ token: string | null }
  | { ok: false; error: string };

/**
 * Mint a short-lived (~1h) installation access token scoped to `owner/repo` —
 * the same two-step the node's webhook clone path runs. `token: null` (with
 * ok) means "App not installed on this repo", the caller's cue to fall back
 * to another credential; `{ok:false}` is a real auth/transport failure that
 * must surface loudly, never silently downgrade to a skipped write.
 */
export async function installationTokenForRepo(
  owner: string,
  repo: string
): Promise<InstallationTokenResult> {
  if (!installationAuthConfigured()) {
    return { ok: false, error: "GITHUB_APP_ID/GITHUB_APP_PRIVATE_KEY not set" };
  }
  let jwt: string;
  try {
    jwt = signAppJwt();
  } catch (e: any) {
    return { ok: false, error: `GITHUB_APP_PRIVATE_KEY is not a valid RSA PEM: ${e?.message || e}` };
  }
  const lookup = await appFetch(
    jwt,
    `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/installation`
  );
  if (lookup.status === 404) return { ok: true, token: null };
  if (lookup.status < 200 || lookup.status >= 300) {
    return { ok: false, error: `installation lookup: ${errOf(lookup.status, lookup.json)}` };
  }
  const id = lookup.json?.id;
  if (typeof id !== "number") return { ok: false, error: "installation lookup returned no id" };
  const mint = await appFetch(jwt, `/app/installations/${id}/access_tokens`, { method: "POST" });
  if (mint.status < 200 || mint.status >= 300) {
    return { ok: false, error: `installation-token mint: ${errOf(mint.status, mint.json)}` };
  }
  const token = mint.json?.token;
  if (typeof token !== "string" || !token) {
    return { ok: false, error: "installation-token mint returned no token" };
  }
  return { ok: true, token };
}
