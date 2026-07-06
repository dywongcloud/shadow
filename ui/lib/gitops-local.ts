"use client";

// Local in-browser GitOps provider (issue #4).
//
// When the team has NOT linked a remote config repo (no GitHub via Composio), we
// still mirror every dashboard CRUD op into GitOps artifacts — but locally, inside
// the browser. This provider reads the live platform objects (the same endpoints
// the server-side sync uses), translates them into the OpenEdge artifact tree
// (`gitops-artifacts.ts` — shared with the server), and materializes that tree as
// a real in-browser git repo (isomorphic-git + LightningFS), committing on every
// change. The result is a versioned, inspectable, downloadable GitOps mirror that
// works with zero external setup. Everything is logged to the console.
//
// When a remote repo IS linked, this is a no-op — `gitops.tsx` pushes to the repo
// instead.

import { apiGet, currentTeam } from "./api";
import { buildArtifacts } from "./gitops-artifacts";
import { ensureRepo, writeFile, unlink, commitAll, cloneRepo, initRepo, listTrackedFiles, push, log as gitLog } from "./isogit";

const BADGE = "background:#8957e5;color:#fff;padding:1px 5px;border-radius:3px;font-weight:600";
const AUTHOR = { name: "OpenEdge GitOps", email: "gitops@openedge.local" };

// localStorage keys for the (optional) GitHub push target. The PAT is shared with
// the manual /git page (`hive_git_pat`); when blank the same-origin git proxy
// injects the connected-GitHub (Composio) token, so a push works either way.
const REMOTE_KEY = "hive_gitops_remote";
const BRANCH_KEY = "hive_gitops_remote_branch";
const PAT_KEY = "hive_git_pat";

export interface LocalSnapshot {
  repo: string; // local repo dir
  team: string;
  files: { path: string; content: string }[];
  commit: string; // short sha of the latest local commit
  projectCount: number;
  syncedAt: number; // epoch ms
  // Remote push state (when a GitHub config repo is configured for the mirror).
  remote?: string; // normalized https clone URL the mirror pushes to
  pushedCommit?: string; // short sha last pushed to the remote
  pushedAt?: number; // epoch ms of the last successful push
  pushError?: string; // last push failure (cleared on success)
}

/** Normalize a user-entered repo into a full https clone URL, or "" if blank.
 *  Accepts `owner/repo`, `github.com/owner/repo`, or a full http(s) URL. */
export function normalizeRepoUrl(raw: string): string {
  const s = (raw || "").trim();
  if (!s) return "";
  let url = s;
  if (/^[\w.-]+\/[\w.-]+$/.test(s)) url = `https://github.com/${s}`; // owner/repo
  else if (!/^https?:\/\//.test(s)) url = `https://${s.replace(/^\/+/, "")}`; // host/owner/repo
  if (!/\.git$/.test(url)) url += ".git";
  return url;
}

/** The configured GitHub push target for the client-side mirror (normalized), or "". */
export function gitopsRemote(): string {
  if (typeof window === "undefined") return "";
  return normalizeRepoUrl(localStorage.getItem(REMOTE_KEY) || "");
}
export function gitopsBranch(): string {
  if (typeof window === "undefined") return "main";
  return (localStorage.getItem(BRANCH_KEY) || "main").trim() || "main";
}
function gitopsToken(): string {
  if (typeof window === "undefined") return "";
  return localStorage.getItem(PAT_KEY) || "";
}
/** Persist (or clear) the mirror's GitHub push target + branch. */
export function setGitopsRemote(url: string, branch = "main"): void {
  if (typeof window === "undefined") return;
  const norm = normalizeRepoUrl(url);
  if (norm) {
    localStorage.setItem(REMOTE_KEY, norm);
    localStorage.setItem(BRANCH_KEY, branch.trim() || "main");
  } else {
    localStorage.removeItem(REMOTE_KEY);
    localStorage.removeItem(BRANCH_KEY);
  }
}

function repoNameFromUrl(url: string): string {
  const m = url.replace(/\.git$/, "").match(/([^/]+)$/);
  return (m ? m[1] : "openedge-gitops").replace(/[^a-zA-Z0-9._-]/g, "-") || "openedge-gitops";
}

/**
 * Resolve the GitHub push target for the autonomous mirror, fully in the
 * background — no UI required:
 *   1. an explicit client-side target (`hive_gitops_remote`, set during
 *      "setup gitops"), else
 *   2. the tenant's linked config repo on the backend (`GET /v1/gitops`, the
 *      repo created during "setup gitops") — auto-discovered and cached so every
 *      change pushes there without the user ever opening a git page.
 * Returns null when GitOps was never set up (→ local-only in-browser fallback).
 */
async function resolveRemote(team: string): Promise<{ url: string; branch: string } | null> {
  const local = gitopsRemote();
  if (local) return { url: local, branch: gitopsBranch() };
  try {
    const link = await apiGet<{ repo?: string; branch?: string; connected?: boolean }>("/v1/gitops");
    if (link?.repo && link.repo.includes("/")) {
      const branch = (link.branch || "main").trim() || "main";
      setGitopsRemote(link.repo, branch); // cache so future syncs skip the lookup
      console.log(`%cgitops%c discovered config repo from setup → ${link.repo} (team ${team})`, BADGE, "color:#8b949e");
      return { url: normalizeRepoUrl(link.repo), branch };
    }
  } catch {
    /* backend unreachable or GitOps not set up → fall back to local-only */
  }
  return null;
}

interface ProjectRow {
  project: string;
  settings?: unknown;
  git?: { repo_url?: string; branch?: string; commit?: string } | null;
  production?: unknown;
  root_dir?: string;
}

function snapKey(team: string): string {
  return `hive_gitops_local_${team}`;
}
function hashKey(team: string): string {
  return `hive_gitops_local_hash_${team}`;
}

/** Whether the local provider is the active GitOps target (no remote repo linked). */
export function localProviderActive(): boolean {
  if (typeof window === "undefined") return false;
  return localStorage.getItem("hive_gitops_linked") !== "1";
}

/** Read the last-materialized local snapshot for a team (for the UI), or null. */
export function readLocalSnapshot(team = currentTeam()): LocalSnapshot | null {
  if (typeof window === "undefined") return null;
  try {
    const raw = localStorage.getItem(snapKey(team));
    return raw ? (JSON.parse(raw) as LocalSnapshot) : null;
  } catch {
    return null;
  }
}

// Stable, dependency-free content hash (FNV-1a) over the artifact tree with
// volatile timestamps stripped — so an unchanged config doesn't re-commit.
function stableHash(files: { path: string; content: string }[]): string {
  const stable = files
    .map((f) => `${f.path}\n${f.content}`)
    .join("\n")
    .replace(/^\s*generatedAt:.*$/gm, "")
    .replace(/\d{4}-\d{2}-\d{2}T[\d:.]+Z/g, "")
    .replace(/_Plan:.*Generated.*_/g, "");
  let h = 0x811c9dc5;
  for (let i = 0; i < stable.length; i++) {
    h ^= stable.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return (h >>> 0).toString(16).padStart(8, "0");
}

let inFlight: Promise<LocalSnapshot | null> | null = null;

/**
 * Regenerate the local GitOps artifact tree from live platform state and commit
 * any change into the in-browser repo. Idempotent (content-hash deduped) and
 * single-flighted. Returns the new snapshot, or null if unchanged / not applicable.
 */
export async function syncLocalGitops(team = currentTeam()): Promise<LocalSnapshot | null> {
  if (typeof window === "undefined") return null;
  if (inFlight) return inFlight;
  inFlight = (async () => {
    try {
      // Pull the live platform objects the artifacts are derived from.
      const [projects, teamInfo, ov] = await Promise.all([
        apiGet<ProjectRow[]>("/v1/gitops/projects").catch(() => [] as ProjectRow[]),
        apiGet<{ name?: string; plan?: string }>(`/v1/teams/${encodeURIComponent(team)}`).catch(() => null),
        apiGet<{ region?: string }>("/v1/overview").catch(() => null),
      ]);
      const projList = Array.isArray(projects) ? projects : [];

      const files = buildArtifacts({
        org: {
          name: teamInfo?.name || (team === "personal" ? "Personal" : team),
          slug: team,
          plan: teamInfo?.plan,
        },
        generatedAt: new Date().toISOString(),
        region: ov?.region,
        projects: projList,
      });

      const hash = stableHash(files);
      const prevHash = localStorage.getItem(hashKey(team));
      const prevSnap = readLocalSnapshot(team);
      const target = await resolveRemote(team);
      const remote = target?.url || "";
      const branch = target?.branch || "main";
      const newPaths = new Set(files.map((f) => f.path));
      const msg = `chore(openedge): sync ${projList.length} project(s) — ${files.length} artifact(s)`;

      // Dedupe: skip only when the artifact tree is unchanged AND any required push
      // already succeeded against the SAME remote (so a previous push failure or a
      // newly-configured remote still triggers a push).
      const pushSatisfied = !remote || (!!prevSnap?.pushedCommit && prevSnap.remote === remote && !prevSnap.pushError);
      if (hash === prevHash && prevSnap && pushSatisfied) {
        console.log(`%cgitops%c local unchanged (${files.length} artifacts, ${projList.length} projects)`, BADGE, "color:#8b949e");
        return null;
      }

      // ---- Push path: a GitHub config repo is configured → clone it, apply the
      // regenerated tree on top of its history, commit, and push (non-destructive;
      // force only as a self-heal if the mirror diverged). ----
      if (remote) {
        console.groupCollapsed(`%cgitops%c remote sync → ${remote} (${branch})`, BADGE, "color:inherit");
        const token = gitopsToken();
        let dir: string;
        let freshRepo = false;
        try {
          dir = await cloneRepo({ url: remote, token, ref: branch, depth: 1 });
        } catch (cloneErr) {
          // Empty/new repo (no commits yet) — start a fresh history for this branch.
          console.log(`%cgitops%c clone failed (${cloneErr instanceof Error ? cloneErr.message : "?"}) → init new repo`, BADGE, "color:#8b949e");
          dir = await initRepo(repoNameFromUrl(remote), branch);
          freshRepo = true;
        }

        // Prune artifact files that no longer exist, then write the current set.
        const tracked = freshRepo ? [] : await listTrackedFiles(dir);
        for (const fp of tracked) {
          if (!newPaths.has(fp)) await unlink(dir, fp);
        }
        for (const f of files) await writeFile(dir, f.path, f.content);

        const sha = await commitAll(dir, msg, AUTHOR);

        try {
          try {
            await push({ dir, url: remote, token, ref: branch });
          } catch (pushErr) {
            const m = pushErr instanceof Error ? pushErr.message : String(pushErr);
            if (/not.*fast.?forward|rejected|failed to update ref|cannot lock ref/i.test(m)) {
              console.log(`%cgitops%c push not fast-forward — force-syncing the mirror`, BADGE, "color:#d29922");
              await push({ dir, url: remote, token, ref: branch, force: true });
            } else {
              throw pushErr;
            }
          }
        } catch (pushErr) {
          // Surface the failure to the UI and DON'T record the content hash, so the
          // next sync retries the push instead of deduping it away.
          const m = pushErr instanceof Error ? pushErr.message : String(pushErr);
          const failSnap: LocalSnapshot = {
            repo: dir, team, files, commit: sha.slice(0, 8),
            projectCount: projList.length, syncedAt: Date.now(),
            remote, pushedCommit: prevSnap?.pushedCommit, pushedAt: prevSnap?.pushedAt,
            pushError: m,
          };
          localStorage.setItem(snapKey(team), JSON.stringify(failSnap));
          console.error(`%cgitops%c push failed → ${remote}`, BADGE, "color:#f85149", m);
          console.groupEnd();
          return failSnap;
        }

        const snap: LocalSnapshot = {
          repo: dir,
          team,
          files,
          commit: sha.slice(0, 8),
          projectCount: projList.length,
          syncedAt: Date.now(),
          remote,
          pushedCommit: sha.slice(0, 8),
          pushedAt: Date.now(),
        };
        localStorage.setItem(snapKey(team), JSON.stringify(snap));
        localStorage.setItem(hashKey(team), hash);
        console.log(`%cgitops%c pushed ${sha.slice(0, 8)} → ${remote.replace(/^https?:\/\//, "").replace(/\.git$/, "")} (${files.length} artifacts)`, BADGE, "color:#3fb950");
        console.groupEnd();
        return snap;
      }

      // ---- Local-only path (no remote configured): materialize in-browser. ----
      const dir = `/openedge-gitops-${team.replace(/[^a-zA-Z0-9._-]/g, "-")}`;
      console.groupCollapsed(`%cgitops%c local sync → ${dir}`, BADGE, "color:inherit");
      const created = await ensureRepo(dir);
      if (created) console.log("initialized new in-browser config repo");

      // Reflect removed projects: unlink artifact files present last time but gone now.
      for (const f of prevSnap?.files || []) {
        if (!newPaths.has(f.path)) await unlink(dir, f.path);
      }
      // Write the current artifact set.
      for (const f of files) await writeFile(dir, f.path, f.content);

      const sha = await commitAll(dir, msg, AUTHOR);

      const snap: LocalSnapshot = {
        repo: dir,
        team,
        files,
        commit: sha.slice(0, 8),
        projectCount: projList.length,
        syncedAt: Date.now(),
      };
      localStorage.setItem(snapKey(team), JSON.stringify(snap));
      localStorage.setItem(hashKey(team), hash);
      console.log(`committed ${sha.slice(0, 8)} — ${files.length} artifacts, ${projList.length} projects`);
      console.groupEnd();
      return snap;
    } catch (e) {
      console.error(`%cgitops%c local sync failed`, BADGE, "color:#f85149", e);
      try {
        console.groupEnd();
      } catch {
        /* not in a group */
      }
      return null;
    } finally {
      inFlight = null;
    }
  })();
  return inFlight;
}

/** Recent local commit history (newest first) for the team's config repo. */
export async function localGitLog(team = currentTeam(), depth = 20) {
  const dir = `/openedge-gitops-${team.replace(/[^a-zA-Z0-9._-]/g, "-")}`;
  try {
    return await gitLog(dir, depth);
  } catch {
    return [];
  }
}
