// Server-side Composio GitHub helper using the Composio **v3 REST API**.
// (The legacy `composio-core` SDK targets v1, which Composio has retired — it
// returns HTTP 410, so we call v3 directly via fetch.)
//
// Degrades gracefully when COMPOSIO_API_KEY is unset so manual git-URL import
// always works.
import "server-only";
import { randomBytes } from "crypto";

const BASE = "https://backend.composio.dev/api/v3";
export const GITHUB_SLUG = "github";

export function composioConfigured(): boolean {
  return !!process.env.COMPOSIO_API_KEY;
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
  try {
    const { owner, repo, path, content, message, branch } = opts;
    const sha = await githubFileSha(entity, owner, repo, path, branch);
    const b64 = Buffer.from(content, "utf-8").toString("base64");
    const res = await ghExec(entity, "GITHUB_CREATE_OR_UPDATE_FILE_CONTENTS", {
      owner, repo, path, message, content: b64,
      ...(branch ? { branch } : {}),
      ...(sha ? { sha } : {}),
    });
    if (res?.successful === false) {
      return { ok: false, error: res?.error || "GitHub commit failed" };
    }
    const data = res?.data?.details ?? res?.data ?? res ?? {};
    const commit = data?.commit?.sha || data?.content?.sha || sha || "";
    return { ok: true, commit };
  } catch (e: any) {
    console.error("composio commitFile failed", e);
    return { ok: false, error: `Composio: ${e?.message || "unknown error"}` };
  }
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
  opts: { name: string; org?: string; isPrivate?: boolean; description?: string }
): Promise<CreateRepoResult> {
  if (!composioConfigured()) return { ok: false, error: "COMPOSIO_API_KEY not set" };
  const { name, org, isPrivate = true, description } = opts;
  const tool = org ? "GITHUB_CREATE_AN_ORGANIZATION_REPOSITORY" : "GITHUB_CREATE_A_REPOSITORY_FOR_THE_AUTHENTICATED_USER";
  const args: Record<string, unknown> = {
    name,
    private: isPrivate,
    auto_init: true,
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
