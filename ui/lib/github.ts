import * as composio from "./composio";
import type { CommitResult, CreateRepoResult, GhRepo, GithubConnectionDetail } from "./composio";
import {
  type CredentialState,
  getGithubAppToken,
  getGithubAppTokenResult,
  githubAppConfigured,
  probeGithubUser,
  readTokenBundle,
  readTokenBundleResult,
  updateBundleMeta,
} from "./github-app";
import * as rest from "./github-rest";
import { installationAuthConfigured, installationTokenForRepo, listAppInstallations } from "./github-installation";

/** GitHub facade, the ONE import for GitHub in the dashboard. Credential order per request: the first-party GitHub App user
 *  token (encrypted cookie); for WRITES then a server-to-server installation token under the App's own identity; then the
 *  legacy Composio connection (`repo` scope only). Names and signatures mirror lib/composio's GitHub helpers; `entity` is
 *  used only by the Composio fallback. */

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
  /** What the connection state came from: the per-browser cookie token (a cache that carries the user's own login), the
   *  App's installation record, or the legacy Composio connection. */
  via?: "user-token" | "installation" | "composio";
  /** Outcome of the App installation-record probe; "error" means genuinely UNKNOWN (see `installationError`), never read as
   *  connected or not, and "unconfigured" means the dashboard has no GITHUB_APP_ID/GITHUB_APP_PRIVATE_KEY. */
  installationState?: "present" | "none" | "error" | "unconfigured";
  installationError?: string;
  /** Accounts (logins) the App is installed on, when installationState==="present". */
  installations?: string[];
  /** Whether THIS request holds a GitHub credential at all (`CredentialState`), independent of whether GitHub was reached.
   *  The two facts are separate on purpose: "we hold a credential" and "GitHub just answered for it" fail for different
   *  reasons, and collapsing them is what made a transient network blip read as "not connected". */
  credential?: CredentialState;
  /** Whether the live probe to GitHub got an ANSWER: "reachable" = it answered for this credential, "unavailable" = no
   *  answer (network/timeout/5xx/rate limit, which says nothing about the credential), "rejected" = it answered and
   *  refused it, "unknown" = the status handler's own deadline beat the probe. */
  reachability?: "reachable" | "unavailable" | "rejected" | "unknown";
  /** True when the answer is honest but incomplete: a credential is held and GitHub could not confirm it just now, so the
   *  UI must say "connected (status check unavailable)" and never "not connected". */
  degraded?: boolean;
  /** Why a probe did not confirm the connection; human-readable and never a token. */
  reachabilityError?: string;
}

/** A secondary GitHub read (orgs, installations) is decoration next to the credential verdict: it must never hold the
 *  answer past the client's own abort, so it races a deadline and falls back to "not fetched". */
const SECONDARY_READ_TIMEOUT_MS = 2500;

async function withTimeout<T>(p: Promise<T>, ms: number, fallback: T): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([p, new Promise<T>((res) => (timer = setTimeout(() => res(fallback), ms)))]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

/** The last known good credential state, with NO network call: for a caller whose own deadline beat GitHub's answer, so an
 *  unanswered probe is reported as "unknown" rather than as "not connected". */
export async function githubLastKnownDetail(entity: string): Promise<GithubDetail> {
  const { bundle, state } = await readTokenBundleResult();
  const held = !!bundle;
  return {
    configured: githubConfigured(),
    connected: held,
    entity,
    login: bundle?.login ?? null,
    scopes: [],
    hasPrivateAccess: held,
    hasOrgScope: false,
    live: false,
    provider: held ? "github-app" : undefined,
    via: held ? "user-token" : undefined,
    credential: state === "ok" ? "valid" : state,
    reachability: "unknown",
    degraded: held,
    ...(held
      ? { reachabilityError: "GitHub did not answer in time — showing the last known connection state." }
      : {}),
  };
}

export async function githubConnectionDetail(entity: string): Promise<GithubDetail> {
  const cred = await getGithubAppTokenResult();
  const held = cred.state === "valid" || cred.state === "refresh-unavailable";

  if (held) {
    // GitHub could not be reached to rotate an expired token: we still HOLD the grant, so the connection is reported as
    // connected-but-unconfirmed. Reporting false here is what made one network blip read as "you are not connected".
    const probe = cred.token ? await probeGithubUser(cred.token) : ({ kind: "unreachable" } as const);

    if (probe.kind === "ok") {
      const token = cred.token as string;
      const [orgs, installs] = await Promise.all([
        withTimeout(rest.ghOrgs(token), SECONDARY_READ_TIMEOUT_MS, [] as { login: string; name?: string }[]),
        withTimeout(rest.ghInstallations(token), SECONDARY_READ_TIMEOUT_MS, []),
      ]);
      const hasOrg = orgs.length > 0 || installs.some((i) => i.account_type === "Organization");
      const slug = installs.find((i) => i.app_slug)?.app_slug;
      const installUrl = slug
        ? `https://github.com/apps/${slug}/installations/new`
        : "https://github.com/settings/installations";
      if (cred.bundle && (cred.bundle.login !== probe.login || (slug && cred.bundle.slug !== slug))) {
        await updateBundleMeta({ login: probe.login, slug }).catch(() => {});
      }
      return {
        configured: true,
        connected: true,
        entity,
        login: probe.login,
        scopes: [],
        hasPrivateAccess: true,
        hasOrgScope: hasOrg,
        live: true,
        provider: "github-app",
        via: "user-token",
        installUrl,
        credential: cred.state,
        reachability: "reachable",
      };
    }

    if (probe.kind === "rejected") {
      // GitHub ANSWERED and refused the credential: the authorization is revoked or stale. The cookie is left for the user
      // to reconnect explicitly — a read never destroys a credential — but this is the one class that is really "not connected".
      return {
        configured: true,
        connected: false,
        entity,
        login: cred.bundle?.login ?? null,
        scopes: [],
        hasPrivateAccess: false,
        hasOrgScope: false,
        live: false,
        provider: "github-app",
        via: "user-token",
        credential: "rejected",
        reachability: "rejected",
        reachabilityError:
          "GitHub refused this connection's token — the authorization was revoked or replaced. Reconnect GitHub to continue.",
      };
    }

    // Unreachable: keep the credential, keep the last known identity, and say the check did not run.
    return {
      configured: true,
      connected: true,
      entity,
      login: cred.bundle?.login ?? null,
      scopes: [],
      hasPrivateAccess: true,
      hasOrgScope: false,
      live: false,
      provider: "github-app",
      via: "user-token",
      credential: cred.state,
      reachability: "unavailable",
      degraded: true,
      reachabilityError: "GitHub could not be reached just now — showing the last known connection state.",
    };
  }

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
    // "foreign"/"unreadable" are reported, not collapsed into "no connection": an undecryptable cookie on a round-robin
    // host whose client secret differs is a deployment fault, and the UI has to be able to say so.
    credential: cred.state,
    ...(d.connected ? { reachability: "reachable" as const } : {}),
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

/** The credential a GitOps WRITE runs under: the request's user-to-server cookie token, else an installation token minted
 *  with the App's own identity for `owner/repo` (so a write without a browser session still applies), else none (the caller
 *  falls back to Composio). `appError` carries why the app path produced no token, and surfaces when nothing else can write. */
async function resolveWriterToken(
  owner: string,
  repo: string
): Promise<{
  token?: string;
  /** Which credential produced the token, reported in write failures: "Resource not accessible by integration" means
   *  different things for a browser cookie (possibly minted against a previous App) and for the App's installation token. */
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

/** The Composio result, upgraded with the app path's reason when that is the real story (the App's reason leads once its
 *  server-to-server identity is configured; Composio's leads when it is the only provider). Never a bare generic error. */
function writerFallback(
  r: CommitResult & { ok: boolean },
  appError?: string
): CommitResult {
  if (r.ok || !appError) return r;
  if (!composio.composioConfigured()) return { ok: false, error: appError };
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
    /** Forwarded to both providers' deletion reconciliation (see `composio.commitFiles`). */
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

/** Turns GitHub's opaque "Resource not accessible by integration" write refusal into something actionable: which repo, which
 *  credential, and whether the App is installed on that owner at all. Best-effort and never throws: a failed diagnosis must
 *  not replace the real error. */
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
  } catch {}
  let hint =
    via === "user-token"
      ? "This used your browser's GitHub authorization; if you recently switched GitHub Apps, sign out and reconnect GitHub so the token is minted against the current App."
      : "This used the App's own installation credential.";
  let scopedToInstallation = false;
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
    } catch {}
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

/** The raw token for private clones (git deploy / redeploy); NEVER returned to the browser. */
export async function githubAccessToken(entity: string): Promise<string | null> {
  const token = await getGithubAppToken();
  if (token) return token;
  return composio.githubAccessToken(entity);
}

/** Disconnects whichever provider is active: the App grant is revoked on GitHub (best-effort) and the encrypted cookie
 *  cleared; Composio connected accounts are all deleted. A later reconnect binds fresh. */
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
