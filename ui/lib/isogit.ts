// Client-side git, in the browser, via isomorphic-git + LightningFS.
//
// Runs the real git smart-HTTP protocol entirely in the browser against an
// in-memory/IndexedDB filesystem. Remote operations (clone/push/pull) go through
// our same-origin proxy (`/api/git/cors-proxy`) so there's no CORS and no
// third-party proxy ever sees the user's token. Authentication uses a PAT the
// user supplies, passed via isomorphic-git's `onAuth` — it is held only in memory
// for the operation and forwarded over our own proxy, never persisted server-side.
//
// All imports are dynamic so this module (and isomorphic-git's bundle) only load
// when the Git page is actually used — zero impact on the rest of the dashboard.

export type Progress = (line: string) => void;

const CORS_PROXY = "/api/git/cors-proxy";

// ---- console logging (issue #4: log all client-side git operations) ----
// Every browser-side git operation logs to the dev console with a consistent
// `[git]` badge, so the full client-side git activity (clone/commit/push/pull/…)
// is observable in DevTools — not just in the page's activity feed. `glog` opens
// a collapsed group per op; `gdone`/`gfail` close it with a timing/result line.
const GIT_BADGE = "background:#1f6feb;color:#fff;padding:1px 5px;border-radius:3px;font-weight:600";
function now(): number {
  return typeof performance !== "undefined" ? performance.now() : 0;
}
function glog(op: string, detail?: Record<string, unknown>): number {
  if (typeof console === "undefined") return now();
  try {
    console.groupCollapsed(`%cgit%c ${op}`, GIT_BADGE, "color:inherit;font-weight:600");
    if (detail && Object.keys(detail).length) console.log(detail);
  } catch {
    /* ignore */
  }
  return now();
}
function gdone(op: string, t0: number, result?: Record<string, unknown>): void {
  if (typeof console === "undefined") return;
  try {
    const ms = Math.round(now() - t0);
    console.log(`%cgit%c ${op} ✓ ${ms}ms`, GIT_BADGE, "color:#3fb950", result ?? "");
    console.groupEnd();
  } catch {
    /* ignore */
  }
}
function gfail(op: string, t0: number, err: unknown): void {
  if (typeof console === "undefined") return;
  try {
    const ms = Math.round(now() - t0);
    console.error(`%cgit%c ${op} ✗ ${ms}ms`, GIT_BADGE, "color:#f85149", err);
    console.groupEnd();
  } catch {
    /* ignore */
  }
}

// One LightningFS instance, lazily created (browser only). `any` at this polyfill
// boundary: LightningFS is a PromiseFsClient that isomorphic-git accepts as `fs`.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let fsInstance: any = null;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
async function getFs(): Promise<any> {
  if (fsInstance) return fsInstance;
  const LightningFS = (await import("@isomorphic-git/lightning-fs")).default;
  fsInstance = new LightningFS("hive-git");
  return fsInstance;
}

async function gitMods() {
  const git = await import("isomorphic-git");
  const http = (await import("isomorphic-git/http/web")).default;
  return { git, http };
}

function authFor(token: string, username = "x-access-token") {
  // GitHub (and most hosts) accept a PAT as the password over Basic auth.
  return () => ({ username: token ? username : "", password: token });
}

export interface RepoIdentity {
  name: string;
  email: string;
}

export interface CloneOpts {
  url: string;
  token?: string;
  ref?: string;
  depth?: number;
  onProgress?: Progress;
}

/** Clone a repo into the browser FS at `/<reponame>`. Returns the working dir. */
export async function cloneRepo(opts: CloneOpts): Promise<string> {
  const fs = await getFs();
  const { git, http } = await gitMods();
  const dir = "/" + repoDirName(opts.url);
  const t0 = glog("clone", { url: opts.url, ref: opts.ref ?? "default", depth: opts.depth ?? 1, dir });
  try {
    await rmrf(dir);
    opts.onProgress?.(`Cloning ${opts.url} …`);
    await git.clone({
      fs,
      http,
      dir,
      url: opts.url,
      corsProxy: CORS_PROXY,
      ref: opts.ref,
      singleBranch: true,
      depth: opts.depth ?? 1,
      onAuth: authFor(opts.token || ""),
      onMessage: (m: string) => {
        const line = m.trim();
        opts.onProgress?.(line);
        if (line && typeof console !== "undefined") console.log(`%cgit%c clone › ${line}`, GIT_BADGE, "color:inherit");
      },
    });
    opts.onProgress?.("Clone complete.");
    gdone("clone", t0, { dir });
    return dir;
  } catch (e) {
    gfail("clone", t0, e);
    throw e;
  }
}

/** Initialize a brand-new repo in the browser FS at `/<name>`. */
export async function initRepo(name: string, defaultBranch = "main"): Promise<string> {
  const fs = await getFs();
  const { git } = await gitMods();
  const dir = "/" + safeName(name);
  const t0 = glog("init", { dir, defaultBranch });
  try {
    await rmrf(dir);
    await git.init({ fs, dir, defaultBranch });
    gdone("init", t0, { dir });
    return dir;
  } catch (e) {
    gfail("init", t0, e);
    throw e;
  }
}

/** Write a file (creating parent dirs) into the working dir. */
export async function writeFile(dir: string, filepath: string, content: string): Promise<void> {
  const fs = (await getFs()) as { promises: { mkdir: (p: string) => Promise<void>; writeFile: (p: string, c: string) => Promise<void> } };
  const full = `${dir}/${filepath}`.replace(/\/+/g, "/");
  const parent = full.slice(0, full.lastIndexOf("/"));
  const t0 = glog("write", { path: full, bytes: content.length });
  try {
    await mkdirp(parent);
    await fs.promises.writeFile(full, content);
    gdone("write", t0, { path: full });
  } catch (e) {
    gfail("write", t0, e);
    throw e;
  }
}

/** Remove a file from the working dir (no-op if absent). Lets the local GitOps
 *  provider reflect deleted projects as removed artifact files before committing. */
export async function unlink(dir: string, filepath: string): Promise<void> {
  const fs = (await getFs()) as { promises: { unlink: (p: string) => Promise<void> } };
  const full = `${dir}/${filepath}`.replace(/\/+/g, "/");
  try {
    await fs.promises.unlink(full);
    if (typeof console !== "undefined") console.log(`%cgit%c rm ${full}`, GIT_BADGE, "color:inherit");
  } catch {
    /* already gone */
  }
}

/** Whether a git repo already exists at `dir` (has a .git dir). */
export async function repoExists(dir: string): Promise<boolean> {
  const fs = (await getFs()) as { promises: { stat: (p: string) => Promise<unknown> } };
  try {
    await fs.promises.stat(`${dir}/.git`);
    return true;
  } catch {
    return false;
  }
}

/** Initialize a repo at `dir` only if one isn't already there (preserves history). */
export async function ensureRepo(dir: string, defaultBranch = "main"): Promise<boolean> {
  const fs = await getFs();
  const { git } = await gitMods();
  if (await repoExists(dir)) return false;
  const t0 = glog("init", { dir, defaultBranch });
  try {
    await git.init({ fs, dir, defaultBranch });
    gdone("init", t0, { dir });
    return true;
  } catch (e) {
    gfail("init", t0, e);
    throw e;
  }
}

/** List the repo's currently-tracked files (from the index/HEAD). Lets the GitOps
 *  mirror prune artifact files that no longer exist before committing a fresh tree
 *  onto a freshly-cloned remote checkout. */
export async function listTrackedFiles(dir: string): Promise<string[]> {
  const fs = await getFs();
  const { git } = await gitMods();
  try {
    return (await git.listFiles({ fs, dir })) as string[];
  } catch {
    return [];
  }
}

/** Read a file from the working dir as text (or null if absent). */
export async function readFile(dir: string, filepath: string): Promise<string | null> {
  const fs = (await getFs()) as { promises: { readFile: (p: string, e: string) => Promise<string> } };
  try {
    return await fs.promises.readFile(`${dir}/${filepath}`.replace(/\/+/g, "/"), "utf8");
  } catch {
    return null;
  }
}

export interface FileStatus {
  filepath: string;
  status: "new" | "modified" | "deleted" | "unmodified";
}

/** Working-tree status (which files changed vs HEAD). */
export async function status(dir: string): Promise<FileStatus[]> {
  const fs = await getFs();
  const { git } = await gitMods();
  const matrix: number[][] & { [i: number]: [string, number, number, number] } =
    (await git.statusMatrix({ fs, dir })) as never;
  const out: FileStatus[] = [];
  for (const row of matrix as unknown as Array<[string, number, number, number]>) {
    const [filepath, head, workdir, stage] = row;
    let s: FileStatus["status"] = "unmodified";
    if (head === 0 && workdir === 2) s = "new";
    else if (head === 1 && workdir === 2) s = "modified";
    else if (head === 1 && workdir === 0) s = "deleted";
    if (s !== "unmodified") out.push({ filepath, status: s });
    void stage;
  }
  if (typeof console !== "undefined") console.log(`%cgit%c status — ${out.length} change(s)`, GIT_BADGE, "color:inherit", out);
  return out;
}

/** Stage all changes and create a commit. Returns the new commit SHA. */
export async function commitAll(dir: string, message: string, author: RepoIdentity): Promise<string> {
  const fs = await getFs();
  const { git } = await gitMods();
  const t0 = glog("commit", { dir, message });
  try {
    const matrix = (await git.statusMatrix({ fs, dir })) as unknown as Array<[string, number, number, number]>;
    let added = 0;
    let removed = 0;
    for (const [filepath, , workdir] of matrix) {
      if (workdir === 0) {
        await git.remove({ fs, dir, filepath });
        removed++;
      } else {
        await git.add({ fs, dir, filepath });
        added++;
      }
    }
    const sha = await git.commit({ fs, dir, message, author: { name: author.name, email: author.email } });
    gdone("commit", t0, { sha: sha.slice(0, 8), staged: added, removed });
    return sha;
  } catch (e) {
    gfail("commit", t0, e);
    throw e;
  }
}

export interface PushOpts {
  dir: string;
  url?: string; // set if no `origin` remote yet (new repo)
  token: string;
  ref?: string;
  force?: boolean;
  onProgress?: Progress;
}

/** Push to `origin` (adding it from `url` first if needed). */
export async function push(opts: PushOpts): Promise<void> {
  const fs = await getFs();
  const { git, http } = await gitMods();
  const t0 = glog("push", { dir: opts.dir, ref: opts.ref ?? "current", force: !!opts.force });
  try {
    if (opts.url) {
      const remotes = await git.listRemotes({ fs, dir: opts.dir });
      if (!remotes.find((r: { remote: string }) => r.remote === "origin")) {
        await git.addRemote({ fs, dir: opts.dir, remote: "origin", url: opts.url });
        if (typeof console !== "undefined") console.log(`%cgit%c push › added remote origin → ${opts.url}`, GIT_BADGE, "color:inherit");
      }
    }
    const ref = opts.ref || (await git.currentBranch({ fs, dir: opts.dir, fullname: false })) || "main";
    opts.onProgress?.(`Pushing ${ref} → origin …`);
    const res = await git.push({
      fs,
      http,
      dir: opts.dir,
      remote: "origin",
      ref,
      corsProxy: CORS_PROXY,
      force: opts.force,
      onAuth: authFor(opts.token),
      onMessage: (m: string) => {
        const line = m.trim();
        opts.onProgress?.(line);
        if (line && typeof console !== "undefined") console.log(`%cgit%c push › ${line}`, GIT_BADGE, "color:inherit");
      },
    });
    if (res?.error) throw new Error(res.error);
    opts.onProgress?.("Push complete.");
    gdone("push", t0, { ref });
  } catch (e) {
    gfail("push", t0, e);
    throw e;
  }
}

/** Recent commit log (newest first). */
export async function log(dir: string, depth = 20): Promise<Array<{ oid: string; message: string; author: string; ts: number }>> {
  const fs = await getFs();
  const { git } = await gitMods();
  const entries = await git.log({ fs, dir, depth });
  if (typeof console !== "undefined") console.log(`%cgit%c log — ${entries.length} commit(s) from ${dir}`, GIT_BADGE, "color:inherit");
  return entries.map((e: { oid: string; commit: { message: string; author: { name: string; timestamp: number } } }) => ({
    oid: e.oid.slice(0, 8),
    message: e.commit.message.split("\n")[0],
    author: e.commit.author.name,
    ts: e.commit.author.timestamp * 1000,
  }));
}

// ---- helpers ----

function repoDirName(url: string): string {
  const m = url.replace(/\.git$/, "").match(/([^/]+)$/);
  return safeName(m ? m[1] : "repo");
}
function safeName(s: string): string {
  return s.trim().replace(/[^a-zA-Z0-9._-]/g, "-").replace(/^-+|-+$/g, "") || "repo";
}
async function mkdirp(dir: string): Promise<void> {
  const fs = (await getFs()) as { promises: { mkdir: (p: string) => Promise<void> } };
  const parts = dir.split("/").filter(Boolean);
  let cur = "";
  for (const p of parts) {
    cur += "/" + p;
    try {
      await fs.promises.mkdir(cur);
    } catch {
      /* exists */
    }
  }
}
async function rmrf(dir: string): Promise<void> {
  const fs = (await getFs()) as {
    promises: {
      readdir: (p: string) => Promise<string[]>;
      stat: (p: string) => Promise<{ isDirectory: () => boolean }>;
      unlink: (p: string) => Promise<void>;
      rmdir: (p: string) => Promise<void>;
    };
  };
  let entries: string[];
  try {
    entries = await fs.promises.readdir(dir);
  } catch {
    return; // doesn't exist
  }
  for (const e of entries) {
    const p = `${dir}/${e}`;
    const st = await fs.promises.stat(p);
    if (st.isDirectory()) await rmrf(p);
    else await fs.promises.unlink(p);
  }
  try {
    await fs.promises.rmdir(dir);
  } catch {
    /* ignore */
  }
}
