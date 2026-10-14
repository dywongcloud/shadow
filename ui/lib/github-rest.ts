// Direct GitHub REST v3 client for first-party GitHub App user-to-server tokens (ghu_...): the token-parameterized twin of
// ./composio with the same return shapes. Server-side only: the token is a bearer credential, never sent to the browser and
// never logged. Every function takes `token` first and degrades (null / [] / { ok:false, error }) instead of throwing.
import "server-only";
import type { GhRepo, CreateRepoResult, CommitResult } from "./composio";

const BASE = "https://api.github.com";
const API_VERSION = "2022-11-28";
const USER_AGENT = "shadw-cloud";

interface GhResponse {
  status: number;
  json: any;
}

/** Minimal GitHub REST v3 fetch returning `{status, json}`; never throws (a network failure is status 0), so a flaky
 *  upstream cannot reject a deploy flow. The token only travels in the Authorization header. */
async function gh(token: string, path: string, init?: RequestInit): Promise<GhResponse> {
  try {
    const r = await fetch(`${BASE}${path}`, {
      ...init,
      headers: {
        Authorization: `Bearer ${token}`,
        Accept: "application/vnd.github+json",
        "X-GitHub-Api-Version": API_VERSION,
        "User-Agent": USER_AGENT,
        "Content-Type": "application/json",
        ...(init?.headers || {}),
      },
      cache: "no-store",
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

function ok(status: number): boolean {
  return status >= 200 && status < 300;
}

/** Best human-readable message from a GitHub error body (message + errors[]). */
function msgOf(json: any, fallback: string): string {
  const errs = Array.isArray(json?.errors)
    ? json.errors
        .map((e: any) => (typeof e === "string" ? e : e?.message))
        .filter(Boolean)
        .join("; ")
    : "";
  return [json?.message, errs].filter(Boolean).join(" — ") || fallback;
}

/** Encode a repo-relative file path per segment, preserving the slashes. */
function encPath(p: string): string {
  return p.split("/").map(encodeURIComponent).join("/");
}

/** Where an org ADMIN installs the GitHub App: for an App (unlike an OAuth app) that is the fix for a 403/404 on org resources. */
function orgInstallUrl(org: string): string {
  return `https://github.com/organizations/${org}/settings/installations`;
}

/** Map raw GitHub repo objects to the dashboard's GhRepo shape (mirrors
 *  composio's mapRepos so both clients are drop-in interchangeable). */
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

/** The authenticated GitHub user's login (used as the default repo owner). */
export async function ghUser(token: string): Promise<{ login: string } | null> {
  const { status, json } = await gh(token, "/user");
  if (!ok(status)) return null;
  return json?.login ? { login: json.login } : null;
}

/** The user's GitHub App installations: a user-to-server token only reaches resources where the App is installed, so this
 *  is the honest map of what is accessible (it powers the repo listing and the org picker). */
export async function ghInstallations(
  token: string
): Promise<{ id: number; app_slug?: string; account_login?: string; account_type?: string }[]> {
  try {
    const { status, json } = await gh(token, "/user/installations?per_page=100");
    if (!ok(status)) return [];
    const arr: any[] = Array.isArray(json?.installations) ? json.installations : [];
    return arr
      .filter((i: any) => typeof i?.id === "number")
      .map((i: any) => ({
        id: i.id,
        app_slug: i.app_slug,
        account_login: i.account?.login,
        account_type: i.account?.type,
      }));
  } catch {
    return [];
  }
}

/** The user's repositories: the UNION of `/user/repos` and each installation's repository list (either can under-return),
 *  deduped by full_name; errors yield []. */
export async function ghRepos(token: string): Promise<GhRepo[]> {
  try {
    const out: GhRepo[] = [];
    const seen = new Set<string>();
    const push = (arr: any[]) => {
      for (const r of mapRepos(arr)) {
        if (!r.full_name || seen.has(r.full_name)) continue;
        seen.add(r.full_name);
        out.push(r);
      }
    };
    const direct = await gh(token, "/user/repos?visibility=all&per_page=100&sort=updated");
    if (ok(direct.status) && Array.isArray(direct.json)) push(direct.json);
    for (const inst of await ghInstallations(token)) {
      const r = await gh(token, `/user/installations/${inst.id}/repositories?per_page=100`);
      if (ok(r.status)) push(r.json?.repositories ?? []);
    }
    return out;
  } catch (e) {
    console.error("github-rest ghRepos failed", e);
    return [];
  }
}

/** The organizations the user can act on: `/user/orgs` unioned with the org-type installation accounts (it can under-return
 *  installation-only orgs), deduped by login; errors yield []. */
export async function ghOrgs(token: string): Promise<{ login: string; name?: string }[]> {
  try {
    const out: { login: string; name?: string }[] = [];
    const seen = new Set<string>();
    const res = await gh(token, "/user/orgs?per_page=100");
    if (ok(res.status) && Array.isArray(res.json)) {
      for (const o of res.json) {
        if (!o?.login || seen.has(o.login)) continue;
        seen.add(o.login);
        out.push({ login: o.login, name: o.name || o.login });
      }
    }
    for (const inst of await ghInstallations(token)) {
      const login = inst.account_login;
      if (inst.account_type === "Organization" && login && !seen.has(login)) {
        seen.add(login);
        out.push({ login });
      }
    }
    return out;
  } catch (e) {
    console.error("github-rest ghOrgs failed", e);
    return [];
  }
}

/** An ORGANIZATION's repositories (`type=all`, private included). A 403/404 almost always means the App is not installed on
 *  the org, so it yields `{restricted, approve_url}` pointing an org admin at the installations page instead of []. */
export async function ghOrgRepos(
  token: string,
  org: string
): Promise<{ repos: GhRepo[]; restricted?: boolean; approve_url?: string; error?: string }> {
  if (!org) return { repos: [] };
  try {
    const { status, json } = await gh(
      token,
      `/orgs/${encodeURIComponent(org)}/repos?type=all&per_page=100&sort=updated`
    );
    if (status === 403 || status === 404) {
      return {
        repos: [],
        restricted: true,
        approve_url: orgInstallUrl(org),
        error: msgOf(json, `HTTP ${status}`),
      };
    }
    if (!ok(status)) return { repos: [], error: msgOf(json, `HTTP ${status}`) };
    return { repos: mapRepos(Array.isArray(json) ? json : []) };
  } catch (e: any) {
    console.error("github-rest ghOrgRepos failed", e);
    return { repos: [], error: e?.message || "org repos failed" };
  }
}

/** Creates a repository for the user or an organization (auto-initialized by default). Never reuses an existing repo: a 422
 *  name-taken answer returns `{ conflict: true }` so the caller retries with a fresh name; an org 403/404 means the App is not
 *  installed there (or lacks repo-creation permission) and yields `{restricted, approve_url}`. */
export async function ghCreateRepo(
  token: string,
  opts: { name: string; org?: string; isPrivate?: boolean; description?: string; autoInit?: boolean }
): Promise<CreateRepoResult> {
  const { name, org, isPrivate = true, description, autoInit = true } = opts;
  const path = org ? `/orgs/${encodeURIComponent(org)}/repos` : "/user/repos";
  try {
    const { status, json } = await gh(token, path, {
      method: "POST",
      body: JSON.stringify({
        name,
        private: isPrivate,
        auto_init: autoInit,
        description: description || "OpenEdge GitOps — auto-managed config",
      }),
    });
    if (ok(status)) {
      return {
        ok: true,
        full_name: json?.full_name || `${org || json?.owner?.login}/${name}`,
        default_branch: json?.default_branch || "main",
      };
    }
    const msg = msgOf(json, `HTTP ${status}`);
    if (status === 422 && /exist|already|name.*taken/i.test(msg)) {
      return { ok: false, conflict: true, error: msg };
    }
    if (org && (status === 403 || status === 404)) {
      return {
        ok: false,
        restricted: true,
        approve_url: orgInstallUrl(org),
        error: `The "${org}" organization doesn't have this GitHub App installed (or it lacks repo-creation permission). Install/approve the app for the organization.`,
      };
    }
    return { ok: false, error: msg };
  } catch (e: any) {
    return { ok: false, error: e?.message || "create repo failed" };
  }
}

/** Installs a repository webhook for push + pull_request events (prod branch -> production, others -> preview); idempotent:
 *  a 422 on a duplicate hook counts as success. */
export async function ghCreateWebhook(
  token: string,
  owner: string,
  repo: string,
  url: string,
  secret?: string
): Promise<{ ok: boolean; error?: string }> {
  try {
    const { status, json } = await gh(
      token,
      `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/hooks`,
      {
        method: "POST",
        body: JSON.stringify({
          name: "web",
          active: true,
          events: ["push", "pull_request"],
          config: {
            url,
            content_type: "json",
            insecure_ssl: "0",
            ...(secret ? { secret } : {}),
          },
        }),
      }
    );
    if (ok(status)) return { ok: true };
    const msg = msgOf(json, `HTTP ${status}`);
    if (status === 422 && /already exists/i.test(msg)) return { ok: true };
    return { ok: false, error: msg || "create webhook failed" };
  } catch (e: any) {
    const msg = String(e?.message || "unknown error");
    if (/already exists/i.test(msg)) return { ok: true };
    return { ok: false, error: msg };
  }
}

/** Sets a GitHub Actions repository VARIABLE (plaintext; no libsodium encryption, unlike secrets): POST first, and on 409/422
 *  (already exists) PATCH the existing one. */
export async function ghSetRepoVariable(
  token: string,
  owner: string,
  repo: string,
  name: string,
  value: string
): Promise<{ ok: boolean; error?: string }> {
  const base = `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/actions/variables`;
  try {
    const created = await gh(token, base, { method: "POST", body: JSON.stringify({ name, value }) });
    if (ok(created.status)) return { ok: true };
    if (created.status === 409 || created.status === 422) {
      const updated = await gh(token, `${base}/${encodeURIComponent(name)}`, {
        method: "PATCH",
        body: JSON.stringify({ name, value }),
      });
      return ok(updated.status)
        ? { ok: true }
        : { ok: false, error: msgOf(updated.json, `HTTP ${updated.status}`) };
    }
    return { ok: false, error: msgOf(created.json, `HTTP ${created.status}`) };
  } catch (e: any) {
    return { ok: false, error: e?.message || "set variable failed" };
  }
}

/** GitHub Contents-API optimistic-concurrency conflict (stale blob SHA). */
function isShaConflict(s: string): boolean {
  return /but expected|does not match|sha.*mismatch/i.test(s || "");
}

/** Blob SHA of an existing file (required to UPDATE via the Contents API),
 *  or null when absent (404 → first-time create, no sha). */
async function ghFileSha(
  token: string,
  owner: string,
  repo: string,
  path: string,
  ref?: string
): Promise<string | null> {
  const qs = ref ? `?ref=${encodeURIComponent(ref)}` : "";
  const { status, json } = await gh(
    token,
    `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/contents/${encPath(path)}${qs}`
  );
  if (!ok(status)) return null;
  return json?.sha || json?.content?.sha || null;
}

const MAX_SHA_CONFLICT_ATTEMPTS = 4;

/** Creates or updates ONE file via the Contents API (PUT, UTF-8 content base64-encoded). The API is optimistic-concurrency
 *  (409 on a stale blob SHA), so each attempt re-reads the fresh SHA and retries up to MAX_SHA_CONFLICT_ATTEMPTS times. */
export async function ghCommitFile(
  token: string,
  opts: { owner: string; repo: string; path: string; content: string; message: string; branch?: string }
): Promise<CommitResult> {
  const { owner, repo, path, content, message, branch } = opts;
  const b64 = Buffer.from(content, "utf-8").toString("base64");
  const contentsPath = `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/contents/${encPath(path)}`;
  let lastErr = "GitHub commit failed";
  for (let attempt = 0; attempt < MAX_SHA_CONFLICT_ATTEMPTS; attempt++) {
    const sha = await ghFileSha(token, owner, repo, path, branch);
    const { status, json } = await gh(token, contentsPath, {
      method: "PUT",
      body: JSON.stringify({
        message,
        content: b64,
        ...(branch ? { branch } : {}),
        ...(sha ? { sha } : {}),
      }),
    });
    if (ok(status)) {
      const commit = json?.content?.sha || json?.commit?.sha || sha || "";
      return { ok: true, commit };
    }
    const msg = msgOf(json, `HTTP ${status}`);
    lastErr = msg;
    if (status === 409 || isShaConflict(msg)) continue;
    return { ok: false, error: msg };
  }
  return { ok: false, error: lastErr };
}

/** Commits MULTIPLE files as ONE atomic commit via the Git Data API (blobs -> tree -> commit -> ref), falling back to per-file
 *  Contents-API commits when a step fails; mirrors `composio.commitFiles` exactly, including `managedPrefixes` tombstoning and
 *  its rule of never writing a tree without a base tree. */
export async function ghCommitFiles(
  token: string,
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
  const { owner, repo, message, files } = opts;
  const branch = opts.branch || "main";
  const managedPrefixes = opts.managedPrefixes || [];
  const repoPath = `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}`;
  let deletions: { path: string; sha: null }[] = [];
  let deletionReconciliationAttempted = false;
  try {
    let baseSha: string | null = null;
    let baseTree: string | null = null;
    const ref = await gh(token, `${repoPath}/git/ref/heads/${encodeURIComponent(branch)}`);
    if (ok(ref.status)) baseSha = ref.json?.object?.sha || ref.json?.sha || null;
    if (baseSha) {
      const commit = await gh(token, `${repoPath}/git/commits/${baseSha}`);
      baseTree = ok(commit.status)
        ? commit.json?.tree?.sha || commit.json?.commit?.tree?.sha || null
        : null;
      if (!baseTree) throw new Error("base tree unresolved on non-empty repo — refusing destructive tree write");
    }

    if (baseTree && managedPrefixes.length) {
      const listing = await gh(token, `${repoPath}/git/trees/${baseTree}?recursive=1`);
      if (!ok(listing.status)) {
        console.warn(
          "github-rest ghCommitFiles: base tree listing failed — skipping deletion reconciliation this sync",
          msgOf(listing.json, `HTTP ${listing.status}`)
        );
      } else if (listing.json?.truncated) {
        console.warn("github-rest ghCommitFiles: base tree listing truncated (large repo) — skipping deletion reconciliation this sync");
      } else {
        const existingPaths: any[] = Array.isArray(listing.json?.tree) ? listing.json.tree : [];
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
        const blob = await gh(token, `${repoPath}/git/blobs`, {
          method: "POST",
          body: JSON.stringify({ content: f.content, encoding: "utf-8" }),
        });
        const sha = ok(blob.status) ? blob.json?.sha : null;
        if (!sha) throw new Error(msgOf(blob.json, "blob create returned no sha"));
        blobShas[i] = sha;
      }
    };
    await Promise.all(
      Array.from({ length: Math.min(BLOB_CONCURRENCY, files.length) }, () => createBlobs())
    );
    const tree: any[] = files.map((f, i) => ({ path: f.path, mode: "100644", type: "blob", sha: blobShas[i] }));
    tree.push(...deletions);

    const t = await gh(token, `${repoPath}/git/trees`, {
      method: "POST",
      body: JSON.stringify({ tree, ...(baseTree ? { base_tree: baseTree } : {}) }),
    });
    const treeSha = ok(t.status) ? t.json?.sha : null;
    if (!treeSha) throw new Error(msgOf(t.json, "tree create returned no sha"));

    const c = await gh(token, `${repoPath}/git/commits`, {
      method: "POST",
      body: JSON.stringify({ message, tree: treeSha, parents: baseSha ? [baseSha] : [] }),
    });
    const commitSha = ok(c.status) ? c.json?.sha : null;
    if (!commitSha) throw new Error(msgOf(c.json, "commit create returned no sha"));

    const moved = baseSha
      ? await gh(token, `${repoPath}/git/refs/heads/${encodeURIComponent(branch)}`, {
          method: "PATCH",
          body: JSON.stringify({ sha: commitSha, force: false }),
        })
      : await gh(token, `${repoPath}/git/refs`, {
          method: "POST",
          body: JSON.stringify({ ref: `refs/heads/${branch}`, sha: commitSha }),
        });
    if (!ok(moved.status)) throw new Error(msgOf(moved.json, "ref update failed"));
    return { ok: true, commit: commitSha };
  } catch (e: any) {
    console.error("github-rest ghCommitFiles atomic path failed, falling back to per-file", e?.message);
    const deletionsSkipped =
      managedPrefixes.length > 0 && (!deletionReconciliationAttempted || deletions.length > 0);
    let last = "";
    for (const f of files) {
      const r = await ghCommitFile(token, { owner, repo, path: f.path, content: f.content, branch, message });
      if (!r.ok) return { ok: false, error: r.error };
      last = r.commit || last;
    }
    return { ok: true, commit: last, ...(deletionsSkipped ? { deletionsSkipped: true } : {}) };
  }
}

/** Revokes the user's authorization (grant) for this App, invalidating ALL of the user's tokens for it (the right semantics
 *  for a dashboard "disconnect"); HTTP Basic with the App's client id/secret. True on 204; never throws, never logs a credential. */
export async function ghRevokeGrant(
  clientId: string,
  clientSecret: string,
  accessToken: string
): Promise<boolean> {
  try {
    const basic = Buffer.from(`${clientId}:${clientSecret}`, "utf-8").toString("base64");
    const r = await fetch(`${BASE}/applications/${encodeURIComponent(clientId)}/grant`, {
      method: "DELETE",
      headers: {
        Authorization: `Basic ${basic}`,
        Accept: "application/vnd.github+json",
        "X-GitHub-Api-Version": API_VERSION,
        "User-Agent": USER_AGENT,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ access_token: accessToken }),
      cache: "no-store",
    });
    return r.status === 204;
  } catch {
    return false;
  }
}
