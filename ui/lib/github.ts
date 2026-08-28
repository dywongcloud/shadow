import * as composio from "./composio";
import type { CommitResult, CreateRepoResult, GhRepo, GithubConnectionDetail } from "./composio";
import { getGithubAppToken, githubAppConfigured, readTokenBundle, updateBundleMeta } from "./github-app";
import * as rest from "./github-rest";
import { installationAuthConfigured, installationTokenForRepo, listAppInstallations } from "./github-installation";

/**
 * GitHub facade — the ONE import for everything GitHub in the dashboard.
 *
 * Credential preference per request:
 *   1. First-party GitHub App user token (encrypted cookie, org-level
 *      permissions — private repos + orgs work wherever the App is installed).
 *   2. READS stop there; WRITES (commit/webhook/variable) additionally fall
 *      through to a server-to-server INSTALLATION token minted with the App's
 *      own identity (./github-installation — the same credential the node's
 *      webhook clone path uses), so a GitOps write triggered without any
 *      browser session still applies instead of silently no-op'ing.
 *   3. Composio-managed OAuth connection (legacy fallback; `repo` scope only,
 *      no org enumeration).
 *
 * CONNECT STATE is derived from GitHub's real installation record whenever the
 * cookie is absent (./github-installation `listAppInstallations`) — the cookie
 * is a cache, never the source of truth.
 *
 * Function names/signatures mirror lib/composio's GitHub helpers exactly, so
 * consumers switch imports without other changes. The `entity` arg is only used
 * by the Composio fallback (the app token is request-scoped via cookie).
 */

export type { CommitResult, CreateRepoResult, GhRepo };
export { resolveEntity, randomConfigRepoName } from "./composio";
export { githubAppConfigured } from "./github-app";

/** Any GitHub auth path available at all (gates connect buttons / GitOps). */
export function githubConfigured(): boolean {
  return githubAppConfigured() || composio.composioConfigured();
}

/** Rich connection detail + which provider is serving it. */
export interface GithubDetail extends GithubConnectionDetail {
  provider?: "github-app" | "composio";
  /** Where to install/configure the App for org access (app provider only). */
  installUrl?: string;
  /**
   * WHAT the connection state is derived from: "user-token" = the per-browser
   * encrypted cookie (a cache — fast, and carries the user's own login);
   * "installation" = the App's REAL installation record, asked of GitHub
   * server-to-server with the App's own identity (the same source the node's
   * webhook/token-resolution path uses) — authoritative, browser-independent;
   * "composio" = the legacy managed OAuth connection.
   */
  via?: "user-token" | "installation" | "composio";
  /**
   * The backend installation-record probe, when app server-to-server auth is
   * in play: "present" (≥1 installation — the record answered CONNECTED),
   * "none" (the record answered: not installed anywhere), "error" (the probe
   * could not be answered — state is genuinely UNKNOWN, see
   * `installationError`; never silently treated as either connected or not),
   * "unconfigured" (no GITHUB_APP_ID/GITHUB_APP_PRIVATE_KEY on the dashboard).
   */
  installationState?: "present" | "none" | "error" | "unconfigured";
  installationError?: string;
  /** Accounts (logins) the App is installed on, when installationState==="present". */
  installations?: string[];
}

export async function githubConnectionDetail(entity: string): Promise<GithubDetail> {
  const token = await getGithubAppToken();
  if (token) {
    const user = await rest.ghUser(token);
    // ONLY a token that GitHub itself just answered for is allowed to decide
    // this. A token can fail here two ways, and only one of them is caught by
    // the expiry check in `getGithubAppToken`: an EXPIRED token is refreshed or
    // cleared there, but a REVOKED one (user removed the authorization, the App
    // was uninstalled and reinstalled, the secret was rotated) keeps a perfectly
    // valid-looking `exp` in the future and is handed back as live.
    //
    // This block used to `return connected: live` unconditionally, so that dead
    // cookie SHORT-CIRCUITED the installation fallback below — the one piece of
    // state that exists precisely to answer "still connected" without a browser.
    // The user-visible result was a permanent "Your GitHub authorization is no
    // longer valid — reconnect" that reconnecting could not durably fix: the new
    // cookie worked until it expired or was revoked, and then the same dead-end
    // returned, with a valid App installation sitting right there unread.
    //
    // So a NOT-live token now falls THROUGH to the installation record instead
    // of reporting a verdict it is not entitled to give.
    if (user?.login) {
      const [orgs, installs] = await Promise.all([rest.ghOrgs(token), rest.ghInstallations(token)]);
      const hasOrg = orgs.length > 0 || installs.some((i) => i.account_type === "Organization");
      const slug = installs.find((i) => i.app_slug)?.app_slug;
      const installUrl = slug
        ? `https://github.com/apps/${slug}/installations/new`
        : "https://github.com/settings/installations";
      // Cache login/slug on the bundle for cheap display next time.
      const bundle = await readTokenBundle();
      if (bundle && (bundle.login !== user.login || (slug && bundle.slug !== slug))) {
        await updateBundleMeta({ login: user.login, slug }).catch(() => {});
      }
      return {
        configured: true,
        connected: true,
        entity,
        login: user.login,
        // GitHub Apps have no OAuth scopes; capabilities come from App permissions
        // + installations. Private-repo access is what the App grants by design.
        scopes: [],
        hasPrivateAccess: true,
        hasOrgScope: hasOrg,
        live: true,
        provider: "github-app",
        via: "user-token",
        installUrl,
      };
    }
  }

  // No usable per-browser cookie (fresh browser, expired, different machine,
  // or — see above — one GitHub just refused).
  //
  // `GET /app/installations` (listAppInstallations) is GLOBAL: it lists EVERY
  // account this GitHub App is installed on across the ENTIRE platform, with
  // no concept of "which user is asking." It is valid evidence for "is the
  // App installed on THIS SPECIFIC repo/org" (installationTokenForRepo scopes
  // it correctly by owner/repo) — it is NEVER valid evidence for "is THIS
  // BROWSER's user connected." Treating a nonempty global list as "connected,
  // login = accounts[0]" reported ANOTHER CUSTOMER's org (whichever happened
  // to be first/only in the list) to any signed-in user with no cookie of
  // their own — a real cross-tenant leak, witnessed live: a fresh account
  // saw "@SolutionsAsService · app installation" despite never having
  // connected anything. Still probe it (below) so `installationState`/
  // `installationError` carry real operator-facing diagnostic signal, but
  // NEVER let its result claim this entity is connected — only a per-entity
  // check (the Composio fallback below, which is genuinely scoped by
  // `entity`) may do that.
  let installationState: GithubDetail["installationState"];
  let installationError: string | undefined;
  if (installationAuthConfigured()) {
    const inst = await listAppInstallations();
    installationState = inst.ok ? (inst.installations.length > 0 ? "present" : "none") : "error";
    if (!inst.ok) installationError = inst.error;
  } else {
    installationState = "unconfigured";
  }

  const d = await composio.githubConnectionDetail(entity);
  return {
    ...d,
    provider: d.connected || d.scopes.length ? "composio" : undefined,
    via: d.connected || d.scopes.length ? "composio" : undefined,
    installationState,
    ...(installationError ? { installationError } : {}),
  };
}

export async function githubStatus(entity: string): Promise<{ connected: boolean; configured: boolean }> {
  const d = await githubConnectionDetail(entity);
  return { connected: d.connected, configured: githubConfigured() };
}

export async function githubUser(entity: string): Promise<{ login: string } | null> {
  const token = await getGithubAppToken();
  if (token) return rest.ghUser(token);
  return composio.githubUser(entity);
}

export async function githubRepos(entity: string): Promise<GhRepo[]> {
  const token = await getGithubAppToken();
  if (token) return rest.ghRepos(token);
  return composio.githubRepos(entity);
}

export async function githubOrgs(entity: string): Promise<{ login: string; name?: string }[]> {
  const token = await getGithubAppToken();
  if (token) return rest.ghOrgs(token);
  return composio.githubOrgs(entity);
}

export async function githubOrgRepos(
  entity: string,
  org: string
): Promise<{ repos: GhRepo[]; restricted?: boolean; approve_url?: string; error?: string }> {
  const token = await getGithubAppToken();
  if (token) return rest.ghOrgRepos(token, org);
  return composio.githubOrgRepos(entity, org);
}

export async function createRepo(
  entity: string,
  opts: { name: string; org?: string; isPrivate?: boolean; description?: string; autoInit?: boolean }
): Promise<CreateRepoResult> {
  const token = await getGithubAppToken();
  if (token) return rest.ghCreateRepo(token, opts);
  return composio.createRepo(entity, opts);
}

/**
 * The credential a GitOps WRITE runs under, in preference order:
 *   1. the request's user-to-server cookie token (user-attributed, when a
 *      signed-in browser is actually present);
 *   2. an installation token minted with the App's OWN server-to-server
 *      identity for `owner/repo` — the same credential the node's webhook
 *      path uses, so a write triggered without any browser session (or from a
 *      browser whose cookie is absent/stale) still APPLIES instead of
 *      silently no-op'ing on "not connected";
 *   3. null → the caller falls back to the legacy Composio connection.
 *
 * `appError` carries the app-credential path's outcome when it produced no
 * token: either a REAL failure (bad key, GitHub 401, transport) or "the App is
 * not installed on this repo" — both surface loudly when no other credential
 * exists, and are appended as context when the legacy fallback also fails.
 */
async function resolveWriterToken(
  owner: string,
  repo: string
): Promise<{
  token?: string;
  /** WHICH credential produced the token — reported in write failures, because
   *  "Resource not accessible by integration" means very different things for a
   *  browser cookie (possibly minted against a previous App) than for the App's
   *  own installation token, and the remedies differ accordingly. */
  via?: "user-token" | "installation";
  appError?: string;
}> {
  const userToken = await getGithubAppToken();
  if (userToken) return { token: userToken, via: "user-token" };
  if (!installationAuthConfigured()) return {};
  const r = await installationTokenForRepo(owner, repo);
  if (r.ok) {
    return r.token
      ? { token: r.token, via: "installation" }
      : {
          appError:
            `The GitHub App is not installed on ${owner} (no installation token for ${owner}/${repo}). ` +
            `Install it on that account or organization at ` +
            `https://github.com/settings/installations, then retry — or pick a repository under an ` +
            `account where it is already installed.`,
        };
  }
  return { appError: r.error };
}

/** composio result, upgraded to the app-credential path's reason when that's
 *  the real story: when Composio isn't even configured its bare
 *  "COMPOSIO_API_KEY not set" would misreport why the write didn't happen;
 *  when it IS configured and failed on its own, the app path's reason is
 *  appended as context. Never a bare silent/generic error. */
function writerFallback(
  r: CommitResult & { ok: boolean },
  appError?: string
): CommitResult {
  if (r.ok || !appError) return r;
  if (!composio.composioConfigured()) return { ok: false, error: appError };
  // WHICH REASON LEADS MATTERS. Composio is the LEGACY provider; once the
  // GitHub App's server-to-server identity is configured, the App is the real
  // one and its reason is the actionable one. This used to lead with Composio's
  // message, so a tenant whose App simply is not installed on the target owner
  // was shown `Composio: Execution of toolkit 'GITHUB' is temporarily disabled
  // by the administrator` — a third party's operational notice, about a
  // provider they are not using, naming nothing they can act on — while the
  // real cause sat after a semicolon.
  //
  // With the App configured, lead with the App's reason and keep Composio's as
  // trailing context (it still explains why the fallback did not rescue the
  // write). With only Composio configured, its message is the whole truth and
  // still leads.
  if (installationAuthConfigured()) {
    return {
      ok: false,
      error: `${appError} (legacy Composio fallback also failed: ${r.error || "composio write failed"})`,
    };
  }
  return { ok: false, error: `${r.error || "composio write failed"}; github-app installation path: ${appError}` };
}

export async function commitFile(
  entity: string,
  opts: { owner: string; repo: string; path: string; content: string; message: string; branch?: string }
): Promise<CommitResult> {
  const { token, appError } = await resolveWriterToken(opts.owner, opts.repo);
  if (token) return rest.ghCommitFile(token, opts);
  return writerFallback(await composio.commitFile(entity, opts), appError);
}

export async function commitFiles(
  entity: string,
  opts: {
    owner: string;
    repo: string;
    branch?: string;
    message: string;
    files: { path: string; content: string }[];
    /** Forwarded to both providers' deletion reconciliation — see
     *  composio.commitFiles' doc comment (rest.ghCommitFiles mirrors the same
     *  logic on the GitHub-App REST path). */
    managedPrefixes?: string[];
  }
): Promise<CommitResult> {
  const { token, via, appError } = await resolveWriterToken(opts.owner, opts.repo);
  if (token) {
    const r = await rest.ghCommitFiles(token, opts);
    return r.ok ? r : { ...r, error: await explainWriteFailure(r.error, opts.owner, opts.repo, via) };
  }
  return writerFallback(await composio.commitFiles(entity, opts), appError);
}

/**
 * Turn GitHub's opaque write refusals into something an operator can act on.
 *
 * "Resource not accessible by integration" is the single most confusing error
 * this integration can produce: it is what GitHub returns for BOTH "the App
 * lacks a permission" and "this credential cannot reach that repository at
 * all", and passed through raw it names neither the repo nor the credential,
 * so it reads as a platform bug. Every write path here therefore says WHICH
 * repo, WHICH credential was used, and — crucially — whether the App is even
 * installed on that owner, which is the usual answer once permissions are
 * granted (an App installed on one account simply cannot see another account's
 * or an org's repos, and a browser cookie minted against a PREVIOUS App is
 * valid-looking but reaches nothing under the current one).
 *
 * Best-effort and never throws: a diagnosis that fails must not replace a real
 * error with its own.
 */
async function explainWriteFailure(
  raw: string | undefined,
  owner: string,
  repo: string,
  via: "user-token" | "installation" | undefined
): Promise<string> {
  const base = raw || "write failed";
  if (!/not accessible by integration|Resource not accessible|404|Not Found/i.test(base)) {
    return `${base} (repo ${owner}/${repo}, credential: ${via ?? "unknown"})`;
  }
  let installedOn = "";
  let orgIsInstalled = false;
  let ownerIsOrg = false;
  try {
    if (installationAuthConfigured()) {
      const inst = await listAppInstallations();
      if (inst.ok) {
        const accounts = inst.installations
          .map((i) => i.account_login)
          .filter((l): l is string => !!l);
        orgIsInstalled = accounts.some((a) => a.toLowerCase() === owner.toLowerCase());
        ownerIsOrg = inst.installations.some(
          (i) => i.account_login?.toLowerCase() === owner.toLowerCase() && i.account_type === "Organization"
        );
        installedOn = accounts.length
          ? `The App is installed on: ${accounts.join(", ")}.`
          : "The App is not installed on ANY account yet.";
        if (accounts.length && !orgIsInstalled) {
          installedOn += ` It is NOT installed on "${owner}", which is why this write was refused.`;
        }
      }
    }
  } catch {
    /* diagnosis is best-effort */
  }
  let hint =
    via === "user-token"
      ? "This used your browser's GitHub authorization; if you recently switched GitHub Apps, sign out and reconnect GitHub so the token is minted against the current App."
      : "This used the App's own installation credential.";
  let scopedToInstallation = false;
  // The App IS installed on this org, so a "not accessible" refusal on a
  // user-token write isn't "wrong App, needs reconnect" — GitHub resolves a
  // user-to-server token's access live per call (no cached installation to
  // mismatch), so the SAME token that reads this org also writes under it.
  // The real gap is the installation's OWN scope: either this repo isn't in
  // its repository selection, or a permission update is pending approval.
  // Probe that directly instead of defaulting every user-token refusal to a
  // reconnect hint that fixes neither case.
  if (via === "user-token" && orgIsInstalled) {
    try {
      const probe = await installationTokenForRepo(owner, repo);
      const settingsUrl = ownerIsOrg
        ? `https://github.com/organizations/${owner}/settings/installations`
        : "https://github.com/settings/installations";
      if (probe.ok && probe.token === null) {
        hint =
          `The App is installed on "${owner}" but not granted access to "${repo}" — an org admin must add this ` +
          `repository under the installation's repository selection at ${settingsUrl}.`;
        scopedToInstallation = true;
      } else if (probe.ok && probe.token) {
        hint =
          `The App's installation on "${owner}" can reach "${repo}" but lacks a permission this write needs — ` +
          `an org admin must approve the pending permission update for the App at ${settingsUrl}.`;
        scopedToInstallation = true;
      }
    } catch {
      /* diagnosis is best-effort — keep the reconnect hint as the fallback */
    }
  }
  return [
    `GitHub refused the write to ${owner}/${repo}: ${base}.`,
    installedOn,
    hint,
    scopedToInstallation ? "" : "Install the App on that account/org, or choose a repository it can reach.",
  ]
    .filter(Boolean)
    .join(" ");
}

export async function createRepoWebhook(
  entity: string,
  owner: string,
  repo: string,
  url: string,
  secret?: string
): Promise<{ ok: boolean; error?: string }> {
  const { token, via, appError } = await resolveWriterToken(owner, repo);
  if (token) {
    const r = await rest.ghCreateWebhook(token, owner, repo, url, secret);
    return r.ok ? r : { ...r, error: await explainWriteFailure(r.error, owner, repo, via) };
  }
  return writerFallback(await composio.createRepoWebhook(entity, owner, repo, url, secret), appError);
}

export async function setRepoVariable(
  entity: string,
  owner: string,
  repo: string,
  name: string,
  value: string
): Promise<{ ok: boolean; error?: string }> {
  const { token, via, appError } = await resolveWriterToken(owner, repo);
  if (token) {
    const r = await rest.ghSetRepoVariable(token, owner, repo, name, value);
    return r.ok ? r : { ...r, error: await explainWriteFailure(r.error, owner, repo, via) };
  }
  return writerFallback(await composio.setRepoVariable(entity, owner, repo, name, value), appError);
}

/**
 * The raw token used server-side for private clones (git deploy / redeploy) and
 * the git proxy. NEVER returned to the browser.
 */
export async function githubAccessToken(entity: string): Promise<string | null> {
  const token = await getGithubAppToken();
  if (token) return token;
  return composio.githubAccessToken(entity);
}

/**
 * Disconnect whichever provider is active: first-party App → best-effort revoke
 * the grant on GitHub + clear the encrypted cookie; Composio → delete every
 * connected account (as before). A later reconnect binds fresh.
 */
export async function disconnectGithub(
  entity: string
): Promise<{ ok: boolean; removed: number; provider?: "github-app" | "composio"; error?: string }> {
  const bundle = await readTokenBundle();
  if (bundle) {
    const { revokeAppGrant, clearTokenCookie } = await import("./github-app");
    await revokeAppGrant(bundle.at).catch(() => false);
    await clearTokenCookie();
    return { ok: true, removed: 1, provider: "github-app" };
  }
  const r = await composio.disconnectGithub(entity);
  return { ...r, provider: "composio" };
}
