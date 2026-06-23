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
    onMessage: (m: string) => opts.onProgress?.(m.trim()),
  });
  opts.onProgress?.("Clone complete.");
  return dir;
}

/** Initialize a brand-new repo in the browser FS at `/<name>`. */
export async function initRepo(name: string, defaultBranch = "main"): Promise<string> {
  const fs = await getFs();
  const { git } = await gitMods();
  const dir = "/" + safeName(name);
  await rmrf(dir);
  await git.init({ fs, dir, defaultBranch });
  return dir;
}

/** Write a file (creating parent dirs) into the working dir. */
export async function writeFile(dir: string, filepath: string, content: string): Promise<void> {
  const fs = (await getFs()) as { promises: { mkdir: (p: string) => Promise<void>; writeFile: (p: string, c: string) => Promise<void> } };
  const full = `${dir}/${filepath}`.replace(/\/+/g, "/");
  const parent = full.slice(0, full.lastIndexOf("/"));
  await mkdirp(parent);
  await fs.promises.writeFile(full, content);
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
  return out;
}

/** Stage all changes and create a commit. Returns the new commit SHA. */
export async function commitAll(dir: string, message: string, author: RepoIdentity): Promise<string> {
  const fs = await getFs();
  const { git } = await gitMods();
  const matrix = (await git.statusMatrix({ fs, dir })) as unknown as Array<[string, number, number, number]>;
  for (const [filepath, , workdir] of matrix) {
    if (workdir === 0) {
      await git.remove({ fs, dir, filepath });
    } else {
      await git.add({ fs, dir, filepath });
    }
  }
  const sha = await git.commit({ fs, dir, message, author: { name: author.name, email: author.email } });
  return sha;
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
  if (opts.url) {
    const remotes = await git.listRemotes({ fs, dir: opts.dir });
    if (!remotes.find((r: { remote: string }) => r.remote === "origin")) {
      await git.addRemote({ fs, dir: opts.dir, remote: "origin", url: opts.url });
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
    onMessage: (m: string) => opts.onProgress?.(m.trim()),
  });
  if (res?.error) throw new Error(res.error);
  opts.onProgress?.("Push complete.");
}

/** Recent commit log (newest first). */
export async function log(dir: string, depth = 20): Promise<Array<{ oid: string; message: string; author: string; ts: number }>> {
  const fs = await getFs();
  const { git } = await gitMods();
  const entries = await git.log({ fs, dir, depth });
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
