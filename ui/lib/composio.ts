// Server-side Composio GitHub helper over the Composio v3 REST API (the v1 `composio-core` SDK is retired: HTTP 410).
// Degrades gracefully when COMPOSIO_API_KEY is unset so manual git-URL import always works.
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

/** The Composio entity for this request: `gh_<clerkUserId>` when signed in, else the `hive_entity` cookie, else `default`.
 *  It must be stable per user so the GitHub link survives reloads, tabs and devices. */
export async function resolveEntity(): Promise<string> {
  if (clerkEnabled) {
    try {
      const { userId } = await auth();
      if (userId) return `gh_${userId}`;
    } catch {}
  }
  const cookieEntity = (await cookies()).get("hive_entity")?.value;
  return cookieEntity || "default";
}

function key(): string {
  return process.env.COMPOSIO_API_KEY || "";
}

/** True for Composio's canned "toolkit temporarily disabled by the administrator" refusal: an account-level state this
 *  repo cannot change, swapped for a message naming something the user or operator can act on. */
export function isToolkitDisabled(msg: unknown): boolean {
  return /temporarily disabled by the administrator/i.test(String(msg ?? ""));
}

/** The actionable rewrite of a toolkit-disabled refusal, for user-facing errors. */
export function toolkitDisabledMessage(toolkit: string): string {
  return (
    `The ${toolkit} integration provider (Composio) has this toolkit disabled at the ` +
    `account level — nothing on your side is wrong. Use the first-party GitHub App ` +
    `connection or a plain git repository URL instead; the platform operator can ` +
    `re-enable the toolkit in the Composio console (app.composio.dev).`
  );
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

const MAX_TOOLKIT_PAGES = 60;
const MAX_TOOLKITS = 2000;

/** List EVERY toolkit/integration available on Composio, following pagination (bounded by MAX_TOOLKIT_PAGES / MAX_TOOLKITS). */
export async function listToolkits(): Promise<Toolkit[]> {
  if (!composioConfigured()) return [];
  try {
    const out: Toolkit[] = [];
    const seen = new Set<string>();
    let cursor: string | null = null;
    for (let page = 0; page < MAX_TOOLKIT_PAGES; page++) {
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
      if (!cursor || items.length === 0 || out.length >= MAX_TOOLKITS) break;
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
): Promise<{ redirectUrl?: string; error?: string; providerDisabled?: boolean }> {
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
    if (url) return { redirectUrl: url };
    const status = (conn?.status || "").toUpperCase();
    if (status === "ACTIVE" || status === "INITIATED") return { redirectUrl };
    return { error: "Composio returned no redirect URL." };
  } catch (e: any) {
    console.error("composio connectToolkit failed", e);
    if (isToolkitDisabled(e?.message)) {
      return { error: toolkitDisabledMessage(slug.toUpperCase()), providerDisabled: true };
    }
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

/** OAuth scopes the platform needs: `repo` (private content), `read:org` (org picker), `workflow` (Actions writes for the
 *  GitOps CI workflow). */
export const GITHUB_SCOPES = ["repo", "read:org", "workflow"] as const;

/** Finds or creates a Composio-managed GitHub auth config and returns its id; `HIVE_GITHUB_AUTH_CONFIG_ID` pins one outright
 *  (it must grant `read:org`: the managed app may not honor a scope override or re-scope an existing config). */
async function githubAuthConfigId(): Promise<string> {
  const pinned = process.env.HIVE_GITHUB_AUTH_CONFIG_ID?.trim();
  if (pinned) return pinned;
  const list = await v3(`/auth_configs?toolkit_slug=${GITHUB_SLUG}`);
  const items: any[] = list?.items ?? [];
  if (items[0]?.id) return items[0].id;
  const created = await v3(`/auth_configs`, {
    method: "POST",
    body: JSON.stringify({
      toolkit: { slug: GITHUB_SLUG },
      auth_config: { type: "use_composio_managed_auth", scopes: GITHUB_SCOPES },
    }),
  });
  return created?.auth_config?.id || created?.id;
}

/** Rich, HONEST view of a GitHub connection — beyond a bare connected bool. */
export interface GithubConnectionDetail {
  configured: boolean;
  /** ACTIVE in Composio AND the token actually works (see `live`). */
  connected: boolean;
  entity: string;
  login: string | null;
  scopes: string[];
  hasPrivateAccess: boolean;
  hasOrgScope: boolean;
  /** Composio says ACTIVE but the token is checked against GitHub — Composio keeps
   *  ACTIVE after GitHub revokes a token, so this catches dead-but-ACTIVE connections. */
  live: boolean;
}

/** Read the ACTIVE connected_account's granted scopes (a space/comma-separated
 *  string on data.scope / state.val.scope). */
function scopesFromAccount(active: any): string[] {
  const raw =
    active?.data?.scope || active?.state?.val?.scope || active?.connectionParams?.scope || "";
  return String(raw)
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** The full GitHub connection state for an entity: `connected` needs the token ACTIVE in Composio AND live against GitHub
 *  (Composio reports ACTIVE for revoked tokens); `scopes`/`login` power the Integrations UI. */
export async function githubConnectionDetail(entity: string): Promise<GithubConnectionDetail> {
  const base: GithubConnectionDetail = {
    configured: composioConfigured(),
    connected: false,
    entity,
    login: null,
    scopes: [],
    hasPrivateAccess: false,
    hasOrgScope: false,
    live: false,
  };
  if (!composioConfigured()) return base;
  try {
    const res = await v3(`/connected_accounts?user_ids=${encodeURIComponent(entity)}&toolkit_slugs=${GITHUB_SLUG}`);
    const items: any[] = res?.items ?? [];
    const active = items.find((x) => (x.status || "").toUpperCase() === "ACTIVE");
    if (!active) return base;
    const scopes = scopesFromAccount(active);
    const user = await githubUser(entity).catch(() => null);
    const live = !!user?.login;
    if (!live) invalidateGithubToken(entity);
    return {
      configured: true,
      connected: live,
      entity,
      login: user?.login ?? null,
      scopes,
      hasPrivateAccess: scopes.includes("repo"),
      hasOrgScope: scopes.includes("read:org"),
      live,
    };
  } catch {
    return { ...base, configured: true };
  }
}

/** Backwards-compatible slim status — now HONEST (connected = ACTIVE + live). */
export async function githubStatus(entity: string): Promise<{ connected: boolean; configured: boolean }> {
  const d = await githubConnectionDetail(entity);
  return { connected: d.connected, configured: d.configured };
}

/** The organizations the connected user belongs to (needs `read:org`). Degrades to
 *  [] when the scope is absent / unconfigured so the UI can fall back gracefully. */
export async function githubOrgs(entity: string): Promise<{ login: string; name?: string }[]> {
  if (!composioConfigured()) return [];
  try {
    const res = await ghExec(entity, "GITHUB_LIST_ORGANIZATIONS_FOR_THE_AUTHENTICATED_USER", { per_page: 100 });
    const data = res?.data?.details ?? res?.data ?? res ?? [];
    const arr: any[] = Array.isArray(data) ? data : data?.organizations ?? data?.items ?? [];
    return arr.map((o: any) => ({ login: o.login, name: o.name || o.login })).filter((o) => o.login);
  } catch (e) {
    console.error("composio orgs failed", e);
    return [];
  }
}

/** Deletes every connected_account this entity has for the toolkit (there can be several), drops the cached token and
 *  returns how many were removed; reconnecting re-consents and so grants new scopes. */
export async function disconnectGithub(entity: string): Promise<{ ok: boolean; removed: number; error?: string }> {
  if (!composioConfigured()) return { ok: false, removed: 0, error: "COMPOSIO_API_KEY not set" };
  try {
    const res = await v3(`/connected_accounts?user_ids=${encodeURIComponent(entity)}&toolkit_slugs=${GITHUB_SLUG}`);
    const items: any[] = res?.items ?? [];
    let removed = 0;
    for (const it of items) {
      if (!it?.id) continue;
      await v3(`/connected_accounts/${encodeURIComponent(it.id)}`, { method: "DELETE" });
      removed++;
    }
    invalidateGithubToken(entity);
    return { ok: true, removed };
  } catch (e: any) {
    return { ok: false, removed: 0, error: e?.message || "disconnect failed" };
  }
}

/** Begin an OAuth connection; returns a redirect URL (or an error message). */
export async function githubConnect(
  entity: string,
  redirectUrl: string
): Promise<{ redirectUrl?: string; error?: string; providerDisabled?: boolean }> {
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
    if (isToolkitDisabled(e?.message)) {
      return { error: toolkitDisabledMessage("GitHub"), providerDisabled: true };
    }
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
    return null;
  }
}

export interface CommitResult {
  ok: boolean;
  commit?: string;
  error?: string;
  /** True when the per-file fallback could not apply pending tombstone deletions (it has no batch-tree-delete): callers must
   *  not treat the sync as fully done, e.g. never persist a content-hash cursor (see ui/app/api/gitops/sync/route.ts). */
  deletionsSkipped?: boolean;
}

const MAX_SHA_CONFLICT_ATTEMPTS = 4;

/** Creates or updates one file (reading the blob SHA first, as the Contents API requires for updates); `content` is UTF-8
 *  text, base64-encoded here. A stale-SHA conflict re-reads the fresh SHA and retries up to MAX_SHA_CONFLICT_ATTEMPTS times. */
export async function commitFile(
  entity: string,
  opts: { owner: string; repo: string; path: string; content: string; message: string; branch?: string }
): Promise<CommitResult> {
  if (!composioConfigured()) return { ok: false, error: "COMPOSIO_API_KEY not set" };
  const { owner, repo, path, content, message, branch } = opts;
  const b64 = Buffer.from(content, "utf-8").toString("base64");
  let lastErr = "GitHub commit failed";
  for (let attempt = 0; attempt < MAX_SHA_CONFLICT_ATTEMPTS; attempt++) {
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
        if (isShaConflict(msg)) continue;
        return { ok: false, error: msg };
      }
      const commit = data?.commit?.sha || data?.content?.sha || sha || "";
      return { ok: true, commit };
    } catch (e: any) {
      const msg = String(e?.message || "unknown error");
      lastErr = msg;
      if (isShaConflict(msg)) continue;
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
  conflict?: boolean;
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

/** Where an org owner approves this OAuth app: a URL parsed from GitHub's 403 body, else the org's third-party-apps policy
 *  page; never empty. */
function orgApproveUrl(org: string, errBody?: string): string {
  const body = errBody || "";
  const doc = body.match(/https:\/\/github\.com\/[^\s"')]+/i)?.[0];
  if (doc && /orgs?\/|policies|settings|oauth/i.test(doc)) return doc;
  return `https://github.com/orgs/${org}/policies/applications`;
}

/** A random, collision-proof name for the GitOps config repo. */
export function randomConfigRepoName(): string {
  return `openedge-gitops-${randomBytes(4).toString("hex")}`;
}

/** Creates the GitOps config repository (`autoInit` gives it a base commit to build on; false creates it EMPTY). Never reuses
 *  an existing repo: a name collision returns `{ conflict: true }` so the caller retries with a fresh name, because GitOps
 *  artifacts must never land on top of someone's real source repo. */
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
      if (looksLikeConflict(msg)) return { ok: false, conflict: true, error: msg };
      if (org && looksLikeOrgRestriction(msg)) {
        return {
          ok: false,
          restricted: true,
          approve_url: orgApproveUrl(org, msg),
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

/** Commits MULTIPLE files as ONE atomic commit via the Git Data API (blobs -> tree -> commit -> ref), falling back to per-file
 *  Contents-API commits when a step is unavailable. With `managedPrefixes`, base-tree paths under those prefixes that are
 *  absent from `files` are tombstoned in the same tree write (best-effort and scoped: never outside the prefixes). */
export async function commitFiles(
  entity: string,
  opts: {
    owner: string;
    repo: string;
    branch?: string;
    message: string;
    files: { path: string; content: string }[];
    /** Path prefixes (e.g. `["projects/"]`) this sync fully owns: a base-tree path under one that is absent from `files` is
     *  tombstoned. Omit or empty = purely additive. */
    managedPrefixes?: string[];
  }
): Promise<CommitResult> {
  if (!composioConfigured()) return { ok: false, error: "COMPOSIO_API_KEY not set" };
  const { owner, repo, message, files } = opts;
  const branch = opts.branch || "main";
  const managedPrefixes = opts.managedPrefixes || [];
  let deletions: { path: string; sha: null }[] = [];
  let deletionReconciliationAttempted = false;
  try {
    let baseSha: string | null = null;
    let baseTree: string | null = null;
    try {
      const ref = await ghExec(entity, "GITHUB_GET_A_REFERENCE", { owner, repo, ref: `heads/${branch}` });
      const ro = deep(ref);
      baseSha = ro?.object?.sha || ro?.sha || null;
    } catch {}
    if (baseSha) {
      const commit = await ghExec(entity, "GITHUB_GET_A_COMMIT", { owner, repo, ref: baseSha });
      const co = deep(commit);
      baseTree = co?.tree?.sha || co?.commit?.tree?.sha || null;
      if (!baseTree) throw new Error("base tree unresolved on non-empty repo — refusing destructive tree write");
    }

    if (baseTree && managedPrefixes.length) {
      try {
        const listing = await ghExec(entity, "GITHUB_GET_A_TREE", { owner, repo, tree_sha: baseTree, recursive: true });
        const lo = deep(listing);
        if (lo?.truncated) {
          console.warn("commitFiles: base tree listing truncated (large repo) — skipping deletion reconciliation this sync");
        } else {
          const existingPaths: any[] = Array.isArray(lo?.tree) ? lo.tree : [];
          const desired = new Set(files.map((f) => f.path));
          for (const entry of existingPaths) {
            const p = entry?.path;
            if (typeof p !== "string" || entry?.type !== "blob") continue;
            if (!managedPrefixes.some((prefix) => p.startsWith(prefix))) continue;
            if (desired.has(p)) continue;
            deletions.push({ path: p, sha: null });
          }
          deletions.sort((a, b) => a.path.localeCompare(b.path));
        }
      } catch (e: any) {
        console.warn("commitFiles: base tree listing failed — skipping deletion reconciliation this sync", e?.message);
      }
    }
    deletionReconciliationAttempted = true;

    const BLOB_CONCURRENCY = 6;
    const blobShas: string[] = new Array(files.length);
    let nextIndex = 0;
    const createBlobs = async () => {
      for (;;) {
        const i = nextIndex++;
        if (i >= files.length) return;
        const f = files[i];
        const blob = await ghExec(entity, "GITHUB_CREATE_A_BLOB", { owner, repo, content: f.content, encoding: "utf-8" });
        const sha = deep(blob)?.sha;
        if (!sha) throw new Error("blob create returned no sha");
        blobShas[i] = sha;
      }
    };
    await Promise.all(
      Array.from({ length: Math.min(BLOB_CONCURRENCY, files.length) }, () => createBlobs())
    );
    const tree: any[] = files.map((f, i) => ({ path: f.path, mode: "100644", type: "blob", sha: blobShas[i] }));
    tree.push(...deletions);

    const treeArgs: Record<string, unknown> = { owner, repo, tree };
    if (baseTree) treeArgs.base_tree = baseTree;
    const t = await ghExec(entity, "GITHUB_CREATE_A_TREE", treeArgs);
    const treeSha = deep(t)?.sha;
    if (!treeSha) throw new Error("tree create returned no sha");

    const c = await ghExec(entity, "GITHUB_CREATE_A_COMMIT", {
      owner, repo, message, tree: treeSha, parents: baseSha ? [baseSha] : [],
    });
    const commitSha = deep(c)?.sha;
    if (!commitSha) throw new Error("commit create returned no sha");

    if (baseSha) {
      await ghExec(entity, "GITHUB_UPDATE_A_REFERENCE", { owner, repo, ref: `heads/${branch}`, sha: commitSha, force: false });
    } else {
      await ghExec(entity, "GITHUB_CREATE_A_REFERENCE", { owner, repo, ref: `refs/heads/${branch}`, sha: commitSha });
    }
    return { ok: true, commit: commitSha };
  } catch (e: any) {
    console.error("commitFiles atomic path failed, falling back to per-file", e?.message);
    const deletionsSkipped =
      managedPrefixes.length > 0 && (!deletionReconciliationAttempted || deletions.length > 0);
    let last = "";
    for (const f of files) {
      const r = await commitFile(entity, { owner, repo, path: f.path, content: f.content, branch, message });
      if (!r.ok) return { ok: false, error: r.error };
      last = r.commit || last;
    }
    return { ok: true, commit: last, ...(deletionsSkipped ? { deletionsSkipped: true } : {}) };
  }
}

/** Sets a GitHub Actions repository VARIABLE (plaintext; no libsodium encryption, unlike secrets): creates it, else updates. */
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

/** Installs a repository webhook so pushes (every branch) and pull-request events reach the node's `/v1/git/webhook`
 *  (prod branch -> production, others -> preview). Idempotent: GitHub's 422 on a duplicate hook counts as success. Preferred
 *  over the Actions workflow, which covers only push-to-main and would double-deploy beside it. */
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
    if (/already exists/i.test(msg)) return await reconcileWebhookSecret(entity, owner, repo, url, secret);
    return { ok: false, error: msg || "create webhook failed" };
  } catch (e: any) {
    const msg = String(e?.message || "unknown error");
    if (/already exists/i.test(msg)) return await reconcileWebhookSecret(entity, owner, repo, url, secret);
    return { ok: false, error: `Composio: ${msg}` };
  }
}

/** An existing hook for `url` must carry the current signing secret or the backend rejects its deliveries (401 "bad
 *  signature"): finds it by `config.url` and PATCHes secret + events + active. */
async function reconcileWebhookSecret(
  entity: string,
  owner: string,
  repo: string,
  url: string,
  secret?: string
): Promise<{ ok: boolean; error?: string }> {
  if (!secret) return { ok: true };
  try {
    const list = await ghExec(entity, "GITHUB_LIST_REPOSITORY_WEBHOOKS", { owner, repo });
    const hooks: any[] = (() => {
      const d = deep(list);
      if (Array.isArray(d)) return d;
      if (Array.isArray(list?.data)) return list.data;
      if (Array.isArray(d?.data)) return d.data;
      return [];
    })();
    const norm = (u: string) => u.replace(/\/+$/, "");
    const mine = hooks.find((h) => norm(String(h?.config?.url || "")) === norm(url));
    if (!mine?.id) {
      return { ok: false, error: "existing webhook found but could not be located to update its secret" };
    }
    const upd = await ghExec(entity, "GITHUB_UPDATE_A_REPOSITORY_WEBHOOK", {
      owner,
      repo,
      hook_id: mine.id,
      active: true,
      events: ["push", "pull_request"],
      config: { url, content_type: "json", insecure_ssl: "0", secret },
    });
    if (upd?.successful === false) {
      return { ok: false, error: String(deep(upd)?.message || upd?.error || "update webhook secret failed") };
    }
    return { ok: true };
  } catch (e: any) {
    return { ok: false, error: `Composio: ${String(e?.message || "reconcile webhook failed")}` };
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

/** Lists an ORGANIZATION's repos (`type=all`, private included) for the org picker; an org restricting third-party OAuth apps
 *  yields `{restricted, approve_url}` instead of a silently swallowed 403. */
export async function githubOrgRepos(
  entity: string,
  org: string
): Promise<{ repos: GhRepo[]; restricted?: boolean; approve_url?: string; error?: string }> {
  if (!composioConfigured() || !org) return { repos: [] };
  try {
    const res = await ghExec(entity, "GITHUB_LIST_ORGANIZATION_REPOSITORIES", {
      org, per_page: 100, sort: "updated", type: "all",
    });
    const d = deep(res);
    if (res?.successful === false || d?.errors) {
      const msg = d?.message || res?.error || JSON.stringify(d?.errors || {});
      if (looksLikeOrgRestriction(msg)) {
        return { repos: [], restricted: true, approve_url: orgApproveUrl(org, msg), error: msg };
      }
      return { repos: [], error: msg };
    }
    const data = res?.data?.details ?? res?.data ?? res ?? [];
    const arr: any[] = Array.isArray(data) ? data : data?.repositories ?? data?.items ?? [];
    return { repos: mapRepos(arr) };
  } catch (e: any) {
    const msg = e?.message || "org repos failed";
    if (looksLikeOrgRestriction(msg)) {
      return { repos: [], restricted: true, approve_url: orgApproveUrl(org, msg), error: msg };
    }
    console.error("composio org repos failed", e);
    return { repos: [], error: msg };
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

const GH_TOKEN_TTL_MS = 60_000;
const ghTokenCache = new Map<string, { token: string | null; expires: number }>();

/** Drop a cached GitHub token (e.g. after an upstream 401) so the next read refetches. */
export function invalidateGithubToken(entity: string): void {
  ghTokenCache.delete(entity);
}

/** The raw GitHub OAuth token of this entity's ACTIVE Composio connection (the credential for private clones), cached for
 *  `GH_TOKEN_TTL_MS`; null when unconfigured, unconnected or not exposed: callers degrade to anonymous git or a pasted PAT. */
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

/** Credential extraction for ANY connected toolkit: the active connection's auth material as a flat map (access_token /
 *  api_key / token / ...) plus a coarse auth `kind`, or null when not connected. */
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
