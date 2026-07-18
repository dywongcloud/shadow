// Direct GitHub REST v3 client for FIRST-PARTY GitHub App user-to-server
// tokens (ghu_…). This is the token-parameterized twin of ./composio: the
// same return shapes, but every call goes straight to api.github.com with a
// caller-supplied token instead of routing through Composio's tool-execution
// API. Server-side only — the token is a bearer credential and must never
// reach the browser (and must NEVER be logged).
//
// Every function takes `token` as its FIRST argument and degrades gracefully
// on failure (null / [] / { ok:false, error }) — a helper that throws breaks
// deploy flows, exactly like the Composio twin.
import "server-only";
import type { GhRepo, CreateRepoResult, CommitResult } from "./composio";

const BASE = "https://api.github.com";
const API_VERSION = "2022-11-28";
// GitHub rejects requests without a User-Agent; use a stable product name.
const USER_AGENT = "shadw-cloud";

interface GhResponse {
  status: number;
  json: any;
}

/**
 * Minimal fetch wrapper for the GitHub REST v3 API. Returns `{status, json}`
 * and NEVER throws on non-2xx (callers branch on status) — nor on network
 * failure (status 0), so a flaky upstream can't take a deploy flow down with
 * an unhandled rejection. The token only ever appears in the Authorization
 * header; it is never logged or echoed back.
 */
async function gh(token: string, path: string, init?: RequestInit): Promise<GhResponse> {
  try {
    const r = await fetch(path.startsWith("http") ? path : `${BASE}${path}`, {
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
    // Network / DNS / abort — surface as status 0 with a message-shaped body
    // so callers' error extraction works uniformly. Never rethrow.
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

/** Where an org ADMIN manages GitHub App installations — for a GitHub App
 *  (unlike an OAuth app) the fix for a 403/404 on org resources is INSTALLING
 *  the app on the org, which happens on this settings page. */
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

/**
 * The user's GitHub App installations (field `installations`). Powers both
 * the installation-scoped repo listing and the org picker: a user-to-server
 * token only reaches resources where the app is installed, so the
 * installation list is the honest map of what's actually accessible.
 */
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

/**
 * List the user's repositories: the UNION of `/user/repos` (everything the
 * user owns/collaborates on that the token can see) and each installation's
 * repository list. The union matters because a user-to-server token can
 * under-return org-owned repos on `/user/repos` while the installation
 * endpoint still surfaces them (and vice versa for personal repos when the
 * app is only installed on an org). Deduped by full_name; errors → [].
 */
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

/**
 * The organizations the user can act on. `/user/orgs` only returns orgs the
 * token is authorized to see and can under-return for installation-only
 * orgs, so we ALSO union in the org-type installation accounts — an org
 * where the app is installed must appear in the picker even if `/user/orgs`
 * omits it. Deduped by login; errors → [].
 */
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

/**
 * List a specific ORGANIZATION's repositories (`type=all` so PRIVATE org
 * repos are included). A 403/404 here almost always means the GitHub App is
 * not installed on the org (GitHub 404s resources the token can't see), so
 * surface `{restricted, approve_url}` pointing an org admin at the
 * installations settings page instead of silently swallowing it → [].
 */
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

/**
 * Create a repository for the user or an organization (auto-initialized by
 * default so the GitOps flow has a base commit/branch to build on).
 *
 * SAFETY (mirrors composio.createRepo): NEVER reuses an existing repo. A 422
 * name-taken response returns `{ conflict: true }` so the caller retries with
 * a fresh random name — GitOps artifacts must never land on a real source
 * repo. An org 403/404 means the GitHub App isn't installed on the org (or
 * lacks repo-creation permission) → `{restricted, approve_url}`.
 */
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
    // Name already taken → signal conflict; DO NOT reuse the existing repo.
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

/**
 * Install a repository webhook delivering push + pull_request events to the
 * OpenEdge node (prod branch → production deploy, other branches / PRs →
 * preview). Idempotent: GitHub 422s a duplicate hook with the same config,
 * which we treat as success — mirrors composio.createRepoWebhook.
 */
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
    // "Hook already exists on this repository" → already installed, ok.
    if (status === 422 && /already exists/i.test(msg)) return { ok: true };
    return { ok: false, error: msg || "create webhook failed" };
  } catch (e: any) {
    const msg = String(e?.message || "unknown error");
    if (/already exists/i.test(msg)) return { ok: true };
    return { ok: false, error: msg };
  }
}

/**
 * Set a GitHub Actions repository VARIABLE (plaintext, e.g.
 * OPENEDGE_WEBHOOK_URL). Variables don't need libsodium encryption like
 * secrets do. Create-then-update: POST first, and on 409/422 (already
 * exists) PATCH the existing variable — same upsert semantics as
 * composio.setRepoVariable.
 */
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
      // Already exists → update in place (PATCH returns 204).
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
  if (!ok(status)) return null; // 404 => file doesn't exist yet
  return json?.sha || json?.content?.sha || null;
}

/**
 * Create or update ONE file via the Contents API (PUT), base64-encoding the
 * UTF-8 content as the API requires. The Contents API is
 * optimistic-concurrency: it 409s ("is at X but expected Y") when the blob
 * SHA we send is stale — the file changed between our SHA read and the write
 * (concurrent syncs, or a just-created base commit). Re-read the FRESH SHA
 * and retry up to 4 times so a transient race resolves itself instead of
 * surfacing raw GitHub JSON to the user — mirrors composio.commitFile's loop.
 */
export async function ghCommitFile(
  token: string,
  opts: { owner: string; repo: string; path: string; content: string; message: string; branch?: string }
): Promise<CommitResult> {
  const { owner, repo, path, content, message, branch } = opts;
  const b64 = Buffer.from(content, "utf-8").toString("base64");
  const contentsPath = `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/contents/${encPath(path)}`;
  let lastErr = "GitHub commit failed";
  for (let attempt = 0; attempt < 4; attempt++) {
    // Fresh SHA every attempt — a stale one is exactly what a conflict means.
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
    if (status === 409 || isShaConflict(msg)) continue; // stale SHA → re-read + retry
    return { ok: false, error: msg };
  }
  return { ok: false, error: lastErr };
}

/**
 * Commit MULTIPLE files as a single atomic commit (the `git add . && commit
 * && push` of the whole artifact tree) via the Git Data API: blobs → tree →
 * commit → update ref. Falls back to per-file Contents-API commits if any
 * Git Data step fails, so artifacts always land — mirrors
 * composio.commitFiles exactly, including its safety rule.
 *
 * Deletion reconciliation: `base_tree` overlays the given `tree` entries onto
 * the existing tree ADDITIVELY — any base_tree path this call doesn't mention
 * survives untouched. That means a per-resource file the caller stops emitting
 * (e.g. a deleted project's `projects/<slug>.yaml`) would otherwise be
 * orphaned in the repo forever. When `managedPrefixes` is given, this lists
 * the base tree (`GET .../git/trees/{sha}?recursive=1`), finds paths under
 * those prefixes that are no longer in `files`, and tombstones them
 * (`sha: null`) in the same tree write — scoped so files outside the managed
 * namespace are never touched. Best-effort: a failed or truncated listing
 * just skips reconciliation for this sync (a delayed-by-one-cycle deletion is
 * fine; a destructive/incorrect one is not). Mirrors composio.commitFiles'
 * identical logic.
 */
export async function ghCommitFiles(
  token: string,
  opts: {
    owner: string;
    repo: string;
    branch?: string;
    message: string;
    files: { path: string; content: string }[];
    /** Path prefixes (e.g. `["projects/"]`) this sync fully owns. A base-tree
     *  path under one of these prefixes that is absent from `files` this time
     *  is treated as stale and removed. Omit/empty to skip deletion
     *  reconciliation (pure additive behavior, the prior default). */
    managedPrefixes?: string[];
  }
): Promise<CommitResult> {
  const { owner, repo, message, files } = opts;
  const branch = opts.branch || "main";
  const managedPrefixes = opts.managedPrefixes || [];
  const repoPath = `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}`;
  // Declared outside the try so the catch-all fallback below (which has no
  // batch-tree-delete equivalent) can tell whether tombstone deletions were
  // computed — or never got the chance to be computed — for this sync, and report
  // `deletionsSkipped` accordingly instead of silently dropping them. Mirrors
  // composio.commitFiles' identical hoisting.
  let deletions: { path: string; sha: null }[] = [];
  let deletionReconciliationAttempted = false;
  try {
    // 1) base ref + tree (absent on a brand-new empty repo)
    let baseSha: string | null = null;
    let baseTree: string | null = null;
    const ref = await gh(token, `${repoPath}/git/ref/heads/${encodeURIComponent(branch)}`);
    if (ok(ref.status)) baseSha = ref.json?.object?.sha || ref.json?.sha || null;
    if (baseSha) {
      const commit = await gh(token, `${repoPath}/git/commits/${baseSha}`);
      baseTree = ok(commit.status)
        ? commit.json?.tree?.sha || commit.json?.commit?.tree?.sha || null
        : null;
      // SAFETY: the branch HAS commits but we couldn't resolve its tree.
      // Building a tree WITHOUT base_tree here would REPLACE the whole repo
      // (delete all other files). Never do that — abort to the
      // non-destructive per-file fallback.
      if (!baseTree) throw new Error("base tree unresolved on non-empty repo — refusing destructive tree write");
    }

    // 1b) deletion reconciliation — see doc comment above. `gh()` never
    // throws, so a failed/truncated listing just leaves `deletions` empty
    // (additive-only for this sync) rather than aborting the commit.
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
    // Reached regardless of whether reconciliation ran, was skipped as
    // inapplicable (no managedPrefixes/baseTree), or failed internally above —
    // in every case `deletions` now holds its final, known value for this sync.
    deletionReconciliationAttempted = true;

    // 2) blobs — bounded-concurrency pool (not unbounded Promise.all: a burst
    // of fully-concurrent requests can trip GitHub's secondary rate limit).
    // Results are written into an index-keyed array so the final tree
    // preserves `files` order regardless of completion order — mirrors
    // composio.commitFiles' identical pool.
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

    // 3) tree (base_tree keeps every file we're not touching)
    const t = await gh(token, `${repoPath}/git/trees`, {
      method: "POST",
      body: JSON.stringify({ tree, ...(baseTree ? { base_tree: baseTree } : {}) }),
    });
    const treeSha = ok(t.status) ? t.json?.sha : null;
    if (!treeSha) throw new Error(msgOf(t.json, "tree create returned no sha"));

    // 4) commit
    const c = await gh(token, `${repoPath}/git/commits`, {
      method: "POST",
      body: JSON.stringify({ message, tree: treeSha, parents: baseSha ? [baseSha] : [] }),
    });
    const commitSha = ok(c.status) ? c.json?.sha : null;
    if (!commitSha) throw new Error(msgOf(c.json, "commit create returned no sha"));

    // 5) move the branch ref to the new commit (create it if the repo was empty)
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
    // The fallback below only ever creates/updates files one at a time via the
    // Contents API — there is no batch-tree-delete equivalent, so any tombstone
    // `deletions` computed above (or that the throw pre-empted before they could be
    // computed at all) are NOT applied here. Surface that to the caller so it can
    // avoid treating this sync as fully reconciled — see `deletionsSkipped` doc.
    const deletionsSkipped =
      managedPrefixes.length > 0 && (!deletionReconciliationAttempted || deletions.length > 0);
    // Fallback: commit each file individually via the Contents API.
    let last = "";
    for (const f of files) {
      const r = await ghCommitFile(token, { owner, repo, path: f.path, content: f.content, branch, message });
      if (!r.ok) return { ok: false, error: r.error };
      last = r.commit || last;
    }
    return { ok: true, commit: last, ...(deletionsSkipped ? { deletionsSkipped: true } : {}) };
  }
}

/**
 * Revoke the user's authorization (grant) for this GitHub App — the "delete
 * an app authorization" endpoint, which invalidates ALL of the user's tokens
 * for the app (the right semantics for a dashboard "disconnect"). Uses HTTP
 * Basic auth with the app's client id/secret; the app-management endpoints
 * live on api.github.com like the rest of REST v3. true on 204, false
 * otherwise; never throws (and never logs any credential).
 */
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
