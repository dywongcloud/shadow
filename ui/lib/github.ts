import * as composio from "./composio";
import type { CommitResult, CreateRepoResult, GhRepo, GithubConnectionDetail } from "./composio";
import { getGithubAppToken, githubAppConfigured, readTokenBundle, updateBundleMeta } from "./github-app";
import * as rest from "./github-rest";

/**
 * GitHub facade — the ONE import for everything GitHub in the dashboard.
 *
 * Provider preference per request:
 *   1. First-party GitHub App user token (encrypted cookie, org-level
 *      permissions — private repos + orgs work wherever the App is installed).
 *   2. Composio-managed OAuth connection (legacy fallback; `repo` scope only,
 *      no org enumeration).
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
}

export async function githubConnectionDetail(entity: string): Promise<GithubDetail> {
  const token = await getGithubAppToken();
  if (token) {
    const user = await rest.ghUser(token);
    const live = !!user?.login;
    let hasOrg = false;
    let installUrl: string | undefined;
    if (live) {
      const [orgs, installs] = await Promise.all([rest.ghOrgs(token), rest.ghInstallations(token)]);
      hasOrg = orgs.length > 0 || installs.some((i) => i.account_type === "Organization");
      const slug = installs.find((i) => i.app_slug)?.app_slug;
      installUrl = slug ? `https://github.com/apps/${slug}/installations/new` : "https://github.com/settings/installations";
      // Cache login/slug on the bundle for cheap display next time.
      const bundle = await readTokenBundle();
      if (bundle && (bundle.login !== user.login || (slug && bundle.slug !== slug))) {
        await updateBundleMeta({ login: user.login, slug }).catch(() => {});
      }
    }
    return {
      configured: true,
      connected: live,
      entity,
      login: user?.login ?? null,
      // GitHub Apps have no OAuth scopes; capabilities come from App permissions
      // + installations. Private-repo access is what the App grants by design.
      scopes: [],
      hasPrivateAccess: live,
      hasOrgScope: hasOrg,
      live,
      provider: "github-app",
      installUrl,
    };
  }
  const d = await composio.githubConnectionDetail(entity);
  return { ...d, provider: d.connected || d.scopes.length ? "composio" : undefined };
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

export async function commitFile(
  entity: string,
  opts: { owner: string; repo: string; path: string; content: string; message: string; branch?: string }
): Promise<CommitResult> {
  const token = await getGithubAppToken();
  if (token) return rest.ghCommitFile(token, opts);
  return composio.commitFile(entity, opts);
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
  const token = await getGithubAppToken();
  if (token) return rest.ghCommitFiles(token, opts);
  return composio.commitFiles(entity, opts);
}

export async function createRepoWebhook(
  entity: string,
  owner: string,
  repo: string,
  url: string,
  secret?: string
): Promise<{ ok: boolean; error?: string }> {
  const token = await getGithubAppToken();
  if (token) return rest.ghCreateWebhook(token, owner, repo, url, secret);
  return composio.createRepoWebhook(entity, owner, repo, url, secret);
}

export async function setRepoVariable(
  entity: string,
  owner: string,
  repo: string,
  name: string,
  value: string
): Promise<{ ok: boolean; error?: string }> {
  const token = await getGithubAppToken();
  if (token) return rest.ghSetRepoVariable(token, owner, repo, name, value);
  return composio.setRepoVariable(entity, owner, repo, name, value);
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
