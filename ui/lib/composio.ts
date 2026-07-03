// Server-side Composio GitHub helper using the Composio **v3 REST API**.
// (The legacy `composio-core` SDK targets v1, which Composio has retired — it
// returns HTTP 410, so we call v3 directly via fetch.)
//
// Degrades gracefully when COMPOSIO_API_KEY is unset so manual git-URL import
// always works.
import "server-only";
import { randomBytes } from "crypto";
import { cookies } from "next/headers";
import { auth } from "@clerk/nextjs/server";

const BASE = "https://backend.composio.dev/api/v3";
export const GITHUB_SLUG = "github";

export function composioConfigured(): boolean {
  return !!process.env.COMPOSIO_API_KEY;
}

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY || !!process.env.CLERK_SECRET_KEY;

/**
 * Resolve the Composio "entity" (user_id) for the current request.
 *
 * The entity scopes a user's GitHub connection + repo access. It MUST be stable
 * for a given user so the connection survives reloads, new tabs and other
 * devices — otherwise the GitOps modal re-prompts and the repo list comes back
 * empty even though GitHub is linked.
 *
 * - Signed-in (Clerk) user  -> `gh_<clerkUserId>` (stable, per-account).
 * - Local / no-auth mode    -> the existing `hive_entity` cookie if present,
 *                              else a single stable `default` entity.
 *
 * This replaces the old per-route random `user-<random>` cookie, which differed
 * between routes (status/connect generated random, repos fell back to "default")
 * and was lost whenever cookies cleared.
 */
export async function resolveEntity(): Promise<string> {
  if (clerkEnabled) {
    try {
      const { userId } = auth();
      if (userId) return `gh_${userId}`;
    } catch {
      /* auth() unavailable (e.g. outside request scope) — fall through */
    }
  }
  const cookieEntity = cookies().get("hive_entity")?.value;
  return cookieEntity || "default";
}

function key(): string {
  return process.env.COMPOSIO_API_KEY || "";
}

async function v3(path: string, init?: RequestInit): Promise<any> {
  const r = await fetch(`${BASE}${path}`, {
    ...init,
    headers: { "x-api-key": key(), "content-type": "application/json", ...(init?.headers || {}) },
    cache: "no-store",
  });
  const text = await r.text();
  let body: any = {};
  try { body = text ? JSON.parse(text) : {}; } catch { body = { raw: text }; }
  if (!r.ok) {
    const msg = body?.error?.message || body?.message || `HTTP ${r.status}`;
    throw new Error(msg);
  }
  return body;
}

export interface Toolkit {
  slug: string;
  name: string;
  logo?: string;
  categories: string[];
  description?: string;
  no_auth: boolean;
}

/** List EVERY toolkit/integration available on Composio, following pagination. */
export async function listToolkits(): Promise<Toolkit[]> {
  if (!composioConfigured()) return [];
  try {
    const out: Toolkit[] = [];
    const seen = new Set<string>();
    let cursor: string | null = null;
    // Safety cap so a misbehaving cursor can't loop forever.
    for (let page = 0; page < 60; page++) {
      const qs = new URLSearchParams({ limit: "100" });
      if (cursor) qs.set("cursor", cursor);
      const res = await v3(`/toolkits?${qs.toString()}`);
      const items: any[] = res?.items ?? res?.toolkits ?? [];
      for (const t of items) {
        const slug = t?.slug;
        if (!slug || seen.has(slug)) continue;
        seen.add(slug);
        const rawCats = t?.meta?.categories ?? t?.categories ?? [];
        const categories: string[] = (Array.isArray(rawCats) ? rawCats : [])
          .map((c: any) => (typeof c === "string" ? c : c?.name))
          .filter(Boolean);
        const authSchemes: any[] = t?.auth_schemes ?? t?.meta?.auth_schemes ?? [];
        const no_auth = t?.no_auth === true || (Array.isArray(authSchemes) && authSchemes.length === 0);
        out.push({
          slug,
          name: t?.name || slug,
          logo: t?.meta?.logo || t?.logo,
          categories,
          description: t?.meta?.description || t?.description,
          no_auth,
        });
      }
      cursor = res?.next_cursor ?? res?.nextCursor ?? null;
      if (!cursor || items.length === 0 || out.length >= 2000) break;
    }
    return out;
  } catch (e) {
    console.error("composio listToolkits failed", e);
    return [];
  }
}

/** Find or create a Composio-managed auth config for a toolkit slug; return its id. */
async function toolkitAuthConfigId(slug: string): Promise<string> {
  const list = await v3(`/auth_configs?toolkit_slug=${encodeURIComponent(slug)}`);
  const items: any[] = list?.items ?? [];
  if (items[0]?.id) return items[0].id;
  const created = await v3(`/auth_configs`, {
    method: "POST",
    body: JSON.stringify({ toolkit: { slug }, auth_config: { type: "use_composio_managed_auth" } }),
  });
  return created?.auth_config?.id || created?.id;
}

/** Begin a connection for an arbitrary toolkit; returns a redirect URL (or error). */
export async function connectToolkit(
  entity: string,
  slug: string,
  redirectUrl: string
): Promise<{ redirectUrl?: string; error?: string }> {
  if (!composioConfigured()) return { error: "COMPOSIO_API_KEY not set" };
  try {
    const authConfigId = await toolkitAuthConfigId(slug);
    const conn = await v3(`/connected_accounts`, {
      method: "POST",
      body: JSON.stringify({
        auth_config: { id: authConfigId },
        connection: { user_id: entity, callback_url: redirectUrl },
      }),
    });
    const url = conn?.redirect_url || conn?.redirect_uri || conn?.connectionData?.val?.redirectUrl;
    // Some toolkits connect without an OAuth redirect (e.g. no_auth / API-key types).
    if (url) return { redirectUrl: url };
    const status = (conn?.status || "").toUpperCase();
    if (status === "ACTIVE" || status === "INITIATED") return { redirectUrl };
    return { error: "Composio returned no redirect URL." };
  } catch (e: any) {
    console.error("composio connectToolkit failed", e);
    return { error: `Composio: ${e?.message || "unknown error"}` };
  }
}

/** Whether this entity has an active connection for the given toolkit slug. */
export async function toolkitStatus(entity: string, slug: string): Promise<{ connected: boolean }> {
  if (!composioConfigured()) return { connected: false };
  try {
    const res = await v3(
      `/connected_accounts?user_ids=${encodeURIComponent(entity)}&toolkit_slugs=${encodeURIComponent(slug)}`
    );
    const items: any[] = res?.items ?? [];
    const connected = items.some((x) => (x.status || "").toUpperCase() === "ACTIVE");
    return { connected };
  } catch {
    return { connected: false };
  }
}

export interface GhRepo {
  name: string;
  full_name: string;
  clone_url: string;
  default_branch: string;
  private: boolean;
  updated_at?: string;
  owner?: string;
}

/** Find or create a Composio-managed GitHub auth config; return its id. */
async function githubAuthConfigId(): Promise<string> {
  const list = await v3(`/auth_configs?toolkit_slug=${GITHUB_SLUG}`);
  const items: any[] = list?.items ?? [];
  if (items[0]?.id) return items[0].id;
  const created = await v3(`/auth_configs`, {
    method: "POST",
    body: JSON.stringify({ toolkit: { slug: GITHUB_SLUG }, auth_config: { type: "use_composio_managed_auth" } }),
  });
  return created?.auth_config?.id || created?.id;
}

/** Whether this entity has an active GitHub connection. */
export async function githubStatus(entity: string): Promise<{ connected: boolean; configured: boolean }> {
  if (!composioConfigured()) return { connected: false, configured: false };
  try {
    const res = await v3(`/connected_accounts?user_ids=${encodeURIComponent(entity)}&toolkit_slugs=${GITHUB_SLUG}`);
    const items: any[] = res?.items ?? [];
    const connected = items.some((x) => (x.status || "").toUpperCase() === "ACTIVE");
    return { connected, configured: true };
  } catch {
    return { connected: false, configured: true };
  }
}

/** Begin an OAuth connection; returns a redirect URL (or an error message). */
export async function githubConnect(
  entity: string,
  redirectUrl: string
): Promise<{ redirectUrl?: string; error?: string }> {
  if (!composioConfigured()) return { error: "COMPOSIO_API_KEY not set" };
  try {
    const authConfigId = await githubAuthConfigId();
    const conn = await v3(`/connected_accounts`, {
      method: "POST",
      body: JSON.stringify({
        auth_config: { id: authConfigId },
        connection: { user_id: entity, callback_url: redirectUrl },
      }),
    });
    const url = conn?.redirect_url || conn?.redirect_uri || conn?.connectionData?.val?.redirectUrl;
    return url ? { redirectUrl: url } : { error: "Composio returned no redirect URL." };
  } catch (e: any) {
    console.error("composio connect failed", e);
    return { error: `Composio: ${e?.message || "unknown error"}` };
  }
}

/** Execute an arbitrary GitHub tool for an entity and return the raw result. */
async function ghExec(entity: string, tool: string, args: Record<string, unknown>): Promise<any> {
  return v3(`/tools/execute/${tool}`, {
    method: "POST",
    body: JSON.stringify({ user_id: entity, arguments: args }),
  });
}

/** The authenticated GitHub user's login (used as the default repo owner). */
export async function githubUser(entity: string): Promise<{ login: string } | null> {
  if (!composioConfigured()) return null;
  try {
    const res = await ghExec(entity, "GITHUB_GET_THE_AUTHENTICATED_USER", {});
    const data = res?.data?.details ?? res?.data ?? res ?? {};
    const login = data?.login || data?.user?.login;
    return login ? { login } : null;
  } catch (e) {
    console.error("composio githubUser failed", e);
    return null;
  }
}

/** Get the blob SHA of an existing file (needed to *update* it), or null if absent. */
async function githubFileSha(entity: string, owner: string, repo: string, path: string, ref?: string): Promise<string | null> {
  try {
    const res = await ghExec(entity, "GITHUB_GET_REPOSITORY_CONTENT", {
      owner, repo, path, ...(ref ? { ref } : {}),
    });
    const data = res?.data?.details ?? res?.data ?? res ?? {};
    return data?.sha || data?.content?.sha || null;
  } catch {
    return null; // 404 => file doesn't exist yet
  }
}

export interface CommitResult {
  ok: boolean;
  commit?: string;
  error?: string;
}

/**
 * Create or update a file in a repo via Composio's GitHub toolkit. Looks up the
 * existing blob SHA first (required by the GitHub contents API for updates) so the
 * same call works for both first-time creation and subsequent config syncs.
 *
 * `content` is plain UTF-8 text; it's base64-encoded here as the API requires.
 */
export async function commitFile(
  entity: string,
  opts: { owner: string; repo: string; path: string; content: string; message: string; branch?: string }
): Promise<CommitResult> {
  if (!composioConfigured()) return { ok: false, error: "COMPOSIO_API_KEY not set" };
  const { owner, repo, path, content, message, branch } = opts;
  const b64 = Buffer.from(content, "utf-8").toString("base64");
  // The Contents API is optimistic-concurrency: it 409s ("is at X but expected Y")
  // when the blob SHA we send is stale — i.e. the file changed between our SHA read
  // and the write (concurrent syncs, or a just-created base commit). Re-read the
  // FRESH SHA and retry a few times so a transient race resolves itself instead of
  // surfacing raw GitHub JSON to the user.
  let lastErr = "GitHub commit failed";
  for (let attempt = 0; attempt < 4; attempt++) {
    try {
      const sha = await githubFileSha(entity, owner, repo, path, branch);
      const res = await ghExec(entity, "GITHUB_CREATE_OR_UPDATE_FILE_CONTENTS", {
        owner, repo, path, message, content: b64,
        ...(branch ? { branch } : {}),
        ...(sha ? { sha } : {}),
      });
      const data = res?.data?.details ?? res?.data ?? res ?? {};
      if (res?.successful === false) {
        const msg = String(data?.message || res?.error || "GitHub commit failed");
        lastErr = msg;
        if (isShaConflict(msg)) continue; // stale SHA → re-read + retry
        return { ok: false, error: msg };
      }
      const commit = data?.commit?.sha || data?.content?.sha || sha || "";
      return { ok: true, commit };
    } catch (e: any) {
      const msg = String(e?.message || "unknown error");
      lastErr = msg;
      if (isShaConflict(msg)) continue; // retry on optimistic-lock conflict
      console.error("composio commitFile failed", e);
      return { ok: false, error: `Composio: ${msg}` };
    }
  }
  return { ok: false, error: lastErr };
}

/** GitHub Contents-API optimistic-concurrency conflict (stale blob SHA). */
function isShaConflict(s: string): boolean {
  return /but expected|does not match|409|sha.*mismatch|is at [0-9a-f]{7,}/i.test(s || "");
}

function deep(res: any): any {
  return res?.data?.details ?? res?.data ?? res ?? {};
}

export interface CreateRepoResult {
  ok: boolean;
  full_name?: string;
  default_branch?: string;
  conflict?: boolean; // the name already exists — caller should retry with a new name
  error?: string;
  /** Set when an ORG has OAuth-App access restrictions enabled: the URL an org
   *  owner must visit to grant this app access. The create can't proceed until then. */
  approve_url?: string;
  restricted?: boolean;
}

/** Detect GitHub's org OAuth-App access-restriction 403 (not a name conflict). */
function looksLikeOrgRestriction(s: string): boolean {
  return /OAuth App access restrictions|restricting-access-to-your-organization|third-parties is limited/i.test(s || "");
}

/** A random, collision-proof name for the GitOps config repo. */
export function randomConfigRepoName(): string {
  // 8 hex chars from crypto — never collides with a user's real source repo.
  return `openedge-gitops-${randomBytes(4).toString("hex")}`;
}

/**
 * Create the GitOps config repository itself (auto-initialized so it has a base
 * commit/branch to build the artifact tree on). Works for a personal account or
 * an organization.
 *
 * SAFETY: this NEVER reuses an existing repository. If the name already exists it
 * returns `{ conflict: true }` so the caller can retry with a fresh random name —
 * we must never commit GitOps artifacts on top of someone's real source repo.
 */
export async function createRepo(
  entity: string,
  opts: { name: string; org?: string; isPrivate?: boolean; description?: string; autoInit?: boolean }
): Promise<CreateRepoResult> {
  if (!composioConfigured()) return { ok: false, error: "COMPOSIO_API_KEY not set" };
  const { name, org, isPrivate = true, description, autoInit = true } = opts;
  const tool = org ? "GITHUB_CREATE_AN_ORGANIZATION_REPOSITORY" : "GITHUB_CREATE_A_REPOSITORY_FOR_THE_AUTHENTICATED_USER";
  const args: Record<string, unknown> = {
    name,
    private: isPrivate,
    // `auto_init: true` (default) gives the GitOps config-repo flow a base commit to
    // build its tree on. The in-browser Git page creates the repo EMPTY (autoInit:false)
    // so its own first local commit is the initial push (no non-fast-forward clash).
    auto_init: autoInit,
    description: description || "OpenEdge GitOps — auto-managed config",
  };
  if (org) args.org = org;
  const looksLikeConflict = (s: string) => /exist|already|name.*taken|422/i.test(s || "");
  try {
    const res = await ghExec(entity, tool, args);
    const d = deep(res);
    if (res?.successful === false || d?.errors) {
      const msg = d?.message || res?.error || JSON.stringify(d?.errors || {});
      // Name already taken → signal conflict; DO NOT reuse the existing repo.
      if (looksLikeConflict(msg)) return { ok: false, conflict: true, error: msg };
      // Org has OAuth-App access restrictions → surface the exact approval URL so an
      // org owner can grant this app, instead of dumping raw GitHub JSON at the user.
      if (org && looksLikeOrgRestriction(msg)) {
        return {
          ok: false,
          restricted: true,
          approve_url: `https://github.com/orgs/${org}/policies/applications`,
          error: `The "${org}" organization restricts third-party OAuth apps. An org owner must approve this app before a repo can be created there.`,
        };
      }
      return { ok: false, error: msg };
    }
    return {
      ok: true,
      full_name: d?.full_name || `${org || d?.owner?.login}/${name}`,
      default_branch: d?.default_branch || "main",
    };
  } catch (e: any) {
    const msg = e?.message || "unknown error";
    if (looksLikeConflict(msg)) return { ok: false, conflict: true, error: msg };
    if (org && looksLikeOrgRestriction(msg)) {
      return {
        ok: false,
        restricted: true,
        approve_url: `https://github.com/orgs/${org}/policies/applications`,
        error: `The "${org}" organization restricts third-party OAuth apps. An org owner must approve this app before a repo can be created there.`,
      };
    }
    return { ok: false, error: `Composio: ${msg}` };
  }
}

/**
 * Commit MULTIPLE files as a single atomic commit (the `git add . && commit &&
 * push` of the whole artifact tree) via the GitHub Git Data API:
 * blobs → tree → commit → update ref. Falls back to per-file Contents-API
 * commits if any Git Data step is unavailable, so artifacts always land.
 */
export async function commitFiles(
  entity: string,
  opts: { owner: string; repo: string; branch?: string; message: string; files: { path: string; content: string }[] }
): Promise<CommitResult> {
  if (!composioConfigured()) return { ok: false, error: "COMPOSIO_API_KEY not set" };
  const { owner, repo, message, files } = opts;
  const branch = opts.branch || "main";
  try {
    // 1) base ref + tree (may be absent on a brand-new empty repo)
    let baseSha: string | null = null;
    let baseTree: string | null = null;
    try {
      const ref = await ghExec(entity, "GITHUB_GET_A_REFERENCE", { owner, repo, ref: `heads/${branch}` });
      const ro = deep(ref);
      baseSha = ro?.object?.sha || ro?.sha || null;
    } catch { /* empty repo */ }
    if (baseSha) {
      const commit = await ghExec(entity, "GITHUB_GET_A_COMMIT", { owner, repo, ref: baseSha });
      const co = deep(commit);
      baseTree = co?.tree?.sha || co?.commit?.tree?.sha || null;
      // SAFETY: the branch HAS commits but we couldn't resolve its tree. Building a
      // tree WITHOUT base_tree here would REPLACE the whole repo (delete all other
      // files). Never do that — abort to the non-destructive per-file fallback.
      if (!baseTree) throw new Error("base tree unresolved on non-empty repo — refusing destructive tree write");
    }

    // 2) blobs
    const tree: any[] = [];
    for (const f of files) {
      const blob = await ghExec(entity, "GITHUB_CREATE_A_BLOB", { owner, repo, content: f.content, encoding: "utf-8" });
      const sha = deep(blob)?.sha;
      if (!sha) throw new Error("blob create returned no sha");
      tree.push({ path: f.path, mode: "100644", type: "blob", sha });
    }

    // 3) tree
    const treeArgs: Record<string, unknown> = { owner, repo, tree };
    if (baseTree) treeArgs.base_tree = baseTree;
    const t = await ghExec(entity, "GITHUB_CREATE_A_TREE", treeArgs);
    const treeSha = deep(t)?.sha;
    if (!treeSha) throw new Error("tree create returned no sha");

    // 4) commit
    const c = await ghExec(entity, "GITHUB_CREATE_A_COMMIT", {
      owner, repo, message, tree: treeSha, parents: baseSha ? [baseSha] : [],
    });
    const commitSha = deep(c)?.sha;
    if (!commitSha) throw new Error("commit create returned no sha");

    // 5) move the branch ref to the new commit (create it if the repo was empty)
    if (baseSha) {
      await ghExec(entity, "GITHUB_UPDATE_A_REFERENCE", { owner, repo, ref: `heads/${branch}`, sha: commitSha, force: false });
    } else {
      await ghExec(entity, "GITHUB_CREATE_A_REFERENCE", { owner, repo, ref: `refs/heads/${branch}`, sha: commitSha });
    }
    return { ok: true, commit: commitSha };
  } catch (e: any) {
    console.error("commitFiles atomic path failed, falling back to per-file", e?.message);
    // Fallback: commit each file individually via the Contents API.
    let last = "";
    for (const f of files) {
      const r = await commitFile(entity, { owner, repo, path: f.path, content: f.content, branch, message });
      if (!r.ok) return { ok: false, error: r.error };
      last = r.commit || last;
    }
    return { ok: true, commit: last };
  }
}

/**
 * Set a GitHub Actions repository VARIABLE (plaintext, e.g. OPENEDGE_WEBHOOK_URL).
 * Variables don't need libsodium encryption like secrets do, and the webhook URL
 * isn't sensitive — so this is the right mechanism. Creates it, or updates if it
 * already exists.
 */
export async function setRepoVariable(
  entity: string,
  owner: string,
  repo: string,
  name: string,
  value: string
): Promise<{ ok: boolean; error?: string }> {
  if (!composioConfigured()) return { ok: false, error: "COMPOSIO_API_KEY not set" };
  try {
    const created = await ghExec(entity, "GITHUB_CREATE_A_REPOSITORY_VARIABLE", { owner, repo, name, value });
    if (created?.successful !== false) return { ok: true };
    // Already exists → update.
    const updated = await ghExec(entity, "GITHUB_UPDATE_A_REPOSITORY_VARIABLE", { owner, repo, name, value });
    return updated?.successful === false ? { ok: false, error: deep(updated)?.message || "set variable failed" } : { ok: true };
  } catch {
    try {
      await ghExec(entity, "GITHUB_UPDATE_A_REPOSITORY_VARIABLE", { owner, repo, name, value });
      return { ok: true };
    } catch (e: any) {
      return { ok: false, error: `Composio: ${e?.message || "unknown error"}` };
    }
  }
}

/**
 * Install a real GitHub repository webhook on a PROJECT's source repo so that
 * pushes (every branch) and pull-request events are delivered to the OpenEdge
 * node, which then creates production/preview deployments (the node's
 * `/v1/git/webhook` classifies: prod branch → production, every other branch / PR
 * → preview). Idempotent: GitHub 422s a duplicate hook with the same config, which
 * we treat as success. Prefer this over the Actions workflow — it covers all
 * branches + PRs (the workflow only fires on push-to-main), and avoids the
 * double-deploy you'd get from running both.
 */
export async function createRepoWebhook(
  entity: string,
  owner: string,
  repo: string,
  url: string,
  secret?: string
): Promise<{ ok: boolean; error?: string }> {
  if (!composioConfigured()) return { ok: false, error: "COMPOSIO_API_KEY not set" };
  try {
    const res = await ghExec(entity, "GITHUB_CREATE_A_REPOSITORY_WEBHOOK", {
      owner,
      repo,
      name: "web",
      active: true,
      events: ["push", "pull_request"],
      config: {
        url,
        content_type: "json",
        insecure_ssl: "0",
        ...(secret ? { secret } : {}),
      },
    });
    if (res?.successful !== false) return { ok: true };
    const msg = String(deep(res)?.message || res?.error || "");
    // "Hook already exists on this repository" → already installed, treat as ok.
    if (/already exists/i.test(msg)) return { ok: true };
    return { ok: false, error: msg || "create webhook failed" };
  } catch (e: any) {
    const msg = String(e?.message || "unknown error");
    if (/already exists/i.test(msg)) return { ok: true };
    return { ok: false, error: `Composio: ${msg}` };
  }
}

function mapRepos(arr: any[]): GhRepo[] {
  return (Array.isArray(arr) ? arr : []).map((r: any) => ({
    name: r.name,
    full_name: r.full_name,
    clone_url: r.clone_url || `https://github.com/${r.full_name}.git`,
    default_branch: r.default_branch || "main",
    private: !!r.private,
    updated_at: r.updated_at,
    owner: r.owner?.login,
  }));
}

/** List repositories for a specific ORGANIZATION (used when the GitOps scope is an
 *  org — the "use existing repo" picker must show the ORG's repos, not the user's). */
export async function githubOrgRepos(entity: string, org: string): Promise<GhRepo[]> {
  if (!composioConfigured() || !org) return [];
  try {
    const res = await ghExec(entity, "GITHUB_LIST_ORGANIZATION_REPOSITORIES", {
      org, per_page: 100, sort: "updated",
    });
    const data = res?.data?.details ?? res?.data ?? res ?? [];
    const arr: any[] = Array.isArray(data) ? data : data?.repositories ?? data?.items ?? [];
    return mapRepos(arr);
  } catch (e) {
    console.error("composio org repos failed", e);
    return [];
  }
}

/** List the connected GitHub user's repositories for this entity. */
export async function githubRepos(entity: string): Promise<GhRepo[]> {
  if (!composioConfigured()) return [];
  try {
    const res = await v3(`/tools/execute/GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER`, {
      method: "POST",
      body: JSON.stringify({ user_id: entity, arguments: { per_page: 100, sort: "updated" } }),
    });
    const data = res?.data?.details ?? res?.data ?? res ?? [];
    const arr: any[] = Array.isArray(data) ? data : data?.repositories ?? data?.items ?? [];
    return arr.map((r: any) => ({
      name: r.name,
      full_name: r.full_name,
      clone_url: r.clone_url || `https://github.com/${r.full_name}.git`,
      default_branch: r.default_branch || "main",
      private: !!r.private,
      updated_at: r.updated_at,
      owner: r.owner?.login,
    }));
  } catch (e) {
    console.error("composio repos failed", e);
    return [];
  }
}

// Short-lived per-entity cache for the GitHub token. A single in-browser git op
// (clone/push) fires several proxied requests in a burst; without a cache each one
// would re-hit Composio. The TTL is deliberately short so a rotated/revoked token
// is picked up quickly; `invalidateGithubToken` drops an entry immediately when a
// cached token is rejected upstream (proxy sees a 401). Module-scope = per server
// process (fine for the Node server the dashboard runs on).
const GH_TOKEN_TTL_MS = 60_000;
const ghTokenCache = new Map<string, { token: string | null; expires: number }>();

/** Drop a cached GitHub token (e.g. after an upstream 401) so the next read refetches. */
export function invalidateGithubToken(entity: string): void {
  ghTokenCache.delete(entity);
}

/**
 * The raw GitHub OAuth access token held by this entity's ACTIVE Composio
 * connection — so server-side code (e.g. the in-browser-git CORS proxy) can use
 * the Composio-managed GitHub link as the git client credential, instead of a
 * separately-pasted PAT. Returns null when Composio is unconfigured, no ACTIVE
 * connection exists, or the token isn't exposed — callers must treat that as
 * "no token" and degrade (public/anonymous git or a user-supplied PAT).
 *
 * Cached per entity for a short TTL (`GH_TOKEN_TTL_MS`) so a burst of git requests
 * shares one Composio lookup; the short TTL + `invalidateGithubToken` keep a
 * rotated/revoked token from lingering.
 */
export async function githubAccessToken(entity: string): Promise<string | null> {
  if (!composioConfigured()) return null;
  const now = Date.now();
  const hit = ghTokenCache.get(entity);
  if (hit && hit.expires > now) return hit.token;
  let token: string | null = null;
  try {
    const res = await v3(
      `/connected_accounts?user_ids=${encodeURIComponent(entity)}&toolkit_slugs=${GITHUB_SLUG}`
    );
    const items: any[] = res?.items ?? [];
    // Prefer an ACTIVE connection; the token lives under `data.access_token` (also
    // seen as `state.val.access_token` on some payloads).
    const active = items.find((x) => (x.status || "").toUpperCase() === "ACTIVE") || items[0];
    const raw =
      active?.data?.access_token ||
      active?.state?.val?.access_token ||
      active?.connectionParams?.access_token ||
      null;
    token = typeof raw === "string" && raw ? raw : null;
  } catch {
    token = null;
  }
  ghTokenCache.set(entity, { token, expires: now + GH_TOKEN_TTL_MS });
  return token;
}

/**
 * Generic credential extraction for ANY connected toolkit — the basis for
 * exposing a Composio-linked integration as a consumable platform resource.
 * Returns the active connection's auth material as a flat map (access_token /
 * api_key / token / etc.) plus a coarse auth `kind`, or null if not connected.
 * Credential shapes vary across toolkits, so we merge the few known carriers.
 */
export async function connectionCredentials(
  entity: string,
  slug: string
): Promise<{ kind: string; credentials: Record<string, string> } | null> {
  if (!composioConfigured()) return null;
  try {
    const res = await v3(
      `/connected_accounts?user_ids=${encodeURIComponent(entity)}&toolkit_slugs=${encodeURIComponent(slug)}`
    );
    const items: any[] = res?.items ?? [];
    const active = items.find((x) => (x.status || "").toUpperCase() === "ACTIVE") || items[0];
    if (!active) return null;
    const sources = [active?.data, active?.state?.val, active?.connectionParams].filter(Boolean);
    const KEYS = [
      "access_token", "token", "bearer_token", "api_key", "apiKey", "key", "secret",
      "client_id", "client_secret", "refresh_token", "password", "username",
      "account_id", "scope", "expires_at", "subdomain", "base_url", "region",
    ];
    const credentials: Record<string, string> = {};
    for (const src of sources) {
      for (const k of KEYS) {
        const v = src?.[k];
        if (typeof v === "string" && v && !(k in credentials)) credentials[k] = v;
      }
    }
    const scheme =
      active?.authConfig?.authScheme ||
      active?.auth_scheme ||
      (credentials.access_token ? "oauth" : credentials.api_key || credentials.apiKey ? "api_key" : "oauth");
    return { kind: String(scheme).toLowerCase(), credentials };
  } catch {
    return null;
  }
}
