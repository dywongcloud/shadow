// GitHub App SERVER-TO-SERVER auth for the dashboard: the App's own identity (an RS256 JWT from GITHUB_APP_ID + GITHUB_APP_PRIVATE_KEY), not the
// per-browser OAuth cookie (./github-app) and not Composio. It is the dashboard's half of the credential the hive-cloud node's webhook path uses
// (crates/hive-cloud/src/github_app_auth.rs: same env names, claims and endpoints), serving `listAppInstallations()` (the platform-wide installation
// record, a diagnostic that never says a given user is connected) and `installationTokenForRepo()` (a short-lived token so GitOps writes apply
// without a user session). Never log or return the private key, a signed JWT or an installation token: errors carry GitHub's status + message only.
import "server-only";
import { createSign } from "crypto";

const API_BASE = "https://api.github.com";
const API_VERSION = "2022-11-28";
const USER_AGENT = "shadw-cloud";
const TIMEOUT_MS = 10_000;
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

/** One authenticated call against the GitHub API with the App JWT; never throws (a network failure is status 0) so callers get one typed-error path. */
async function appFetch(
  jwt: string,
  path: string,
  init?: RequestInit
): Promise<{ status: number; json: any }> {
  try {
    const r = await fetch(`${API_BASE}${path}`, {
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

/** The App's REAL installation record (`GET /app/installations`): every account the App is installed on, platform-wide. An empty list is a real
 *  "not installed anywhere"; `{ok:false}` means the question could not be asked (bad key, GitHub down) and the state is UNKNOWN. */
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

/** Mints a short-lived (~1h) installation token scoped to `owner/repo` (the node's webhook clone path runs the same two steps). `token: null` with
 *  ok means "App not installed on this repo" (the cue to fall back to another credential); `{ok:false}` is a real failure that must surface loudly. */
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
