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
import { ensureRepo, writeFile, unlink, commitAll, log as gitLog } from "./isogit";

const BADGE = "background:#8957e5;color:#fff;padding:1px 5px;border-radius:3px;font-weight:600";
const AUTHOR = { name: "OpenEdge GitOps", email: "gitops@openedge.local" };

export interface LocalSnapshot {
  repo: string; // local repo dir
  team: string;
  files: { path: string; content: string }[];
  commit: string; // short sha of the latest local commit
  projectCount: number;
  syncedAt: number; // epoch ms
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
  if (!localProviderActive()) return null; // remote repo linked → handled elsewhere
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
      if (hash === prevHash && prevSnap) {
        console.log(`%cgitops%c local unchanged (${files.length} artifacts, ${projList.length} projects)`, BADGE, "color:#8b949e");
        return null;
      }

      const dir = `/openedge-gitops-${team.replace(/[^a-zA-Z0-9._-]/g, "-")}`;
      console.groupCollapsed(`%cgitops%c local sync → ${dir}`, BADGE, "color:inherit");
      const created = await ensureRepo(dir);
      if (created) console.log("initialized new in-browser config repo");

      // Reflect removed projects: unlink artifact files present last time but gone now.
      const newPaths = new Set(files.map((f) => f.path));
      for (const f of prevSnap?.files || []) {
        if (!newPaths.has(f.path)) await unlink(dir, f.path);
      }
      // Write the current artifact set.
      for (const f of files) await writeFile(dir, f.path, f.content);

      const msg = `chore(openedge): sync ${projList.length} project(s) — ${files.length} artifact(s)`;
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
