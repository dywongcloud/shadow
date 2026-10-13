"use client";

// Persistent store for IN-FLIGHT deployments (builds without a real deployment record yet), kept in localStorage so a "Building" row survives
// navigation and reload. An entry enters on deploy (queued/building) and is removed by the global poller (`PendingBuildsProvider`) once its build
// is terminal, when the real row appears from `/deployments`; a stale-prune backstops builds that never resolve.

import { useEffect, useState } from "react";
import { currentTeam } from "@/lib/api";

export interface PendingBuild {
  /** The build id (also the `/deploy/:id` route + `/v1/builds/:id` key). */
  id: string;
  project: string;
  team: string;
  env: "production" | "preview";
  /** Epoch ms this build was started (for "just now" + stale-prune). */
  at: number;
  /** Epoch ms the build reached READY. The row is kept for a grace window after: the real record lives on the node that ran the build and
   *  `/deployments` reads through round-robin DNS, so for a beat NEITHER row exists on the node polled; removing on `ready` made it blink out. */
  settledAt?: number;
  /** The REAL deployment record's id (stamped from the build's `deployment_id` at ready): the merge key, dropped exactly when a deployment with
   *  this id is visible in the rendered list. Ids are server-minted so the comparison is clock-free; a project+time heuristic breaks under clock
   *  skew and mis-supersedes concurrent redeploys of one project. */
  deploymentId?: string;
}

const KEY = "hive_pending_builds";
const EVT = "hive-pending-builds";

function read(): PendingBuild[] {
  if (typeof window === "undefined") return [];
  try {
    const v = JSON.parse(localStorage.getItem(KEY) || "[]");
    return Array.isArray(v) ? (v as PendingBuild[]) : [];
  } catch {
    return [];
  }
}

function write(list: PendingBuild[]): void {
  if (typeof window === "undefined") return;
  localStorage.setItem(KEY, JSON.stringify(list));
  window.dispatchEvent(new Event(EVT));
}

/** All pending builds (every team) — for the background poller. */
export function getPendingBuilds(): PendingBuild[] {
  return read();
}

/** Record a newly-started deployment so its "Building" row persists. Upserts by id. */
export function addPendingBuild(b: Omit<PendingBuild, "at">): void {
  const list = read().filter((x) => x.id !== b.id);
  list.unshift({ ...b, at: Date.now() });
  write(list);
}

/** Marks a build finished-and-ready without removing its row: it keeps rendering as "Finalizing" until the real deployment record shows up in
 *  the rendered list or `SETTLED_GRACE_MS` elapses. */
export function markPendingSettled(id: string, deploymentId?: string | null): void {
  const list = read();
  let changed = false;
  const next = list.map((x) => {
    if (x.id !== id || x.settledAt) return x;
    changed = true;
    return { ...x, settledAt: Date.now(), ...(deploymentId ? { deploymentId } : {}) };
  });
  if (changed) write(next);
}

/** Re-homes rows recorded under the unresolved `__pending__` tenant (identity sync still in flight at deploy time, which matches EVERY
 *  viewer's filter) to the tenant the session has since resolved to. */
export function rehomePendingTeam(): void {
  const t = currentTeam();
  if (!t || t === "__pending__") return;
  const list = read();
  let changed = false;
  const next = list.map((x) => {
    if (x.team && x.team !== "__pending__") return x;
    changed = true;
    return { ...x, team: t };
  });
  if (changed) write(next);
}

/** Drop a pending build (build finished, or being pruned). */
export function removePendingBuild(id: string): void {
  const list = read();
  const next = list.filter((x) => x.id !== id);
  if (next.length !== list.length) write(next);
}

/** Whether a tenant value is actually KNOWN: `currentTeam()` returns `PENDING_TENANT` (and an empty string before hydration) while identity
 *  resolves, which means "not known yet", never "does not match" (treating them alike made a deploying user's own row vanish on reload). */
function teamKnown(t: string): boolean {
  return !!t && t !== "__pending__";
}

/** How long a finished (ready) row keeps rendering while the real record propagates. */
export const SETTLED_GRACE_MS = 90_000;

/** The current team's pending builds (optionally one project), live on same-tab changes, cross-tab `storage` events and team switches; never
 *  hides a row merely because the tenant is still resolving. */
export function usePendingBuilds(opts?: { project?: string }): PendingBuild[] {
  const [list, setList] = useState<PendingBuild[]>([]);
  const [team, setTeam] = useState<string>("");
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    let lastRaw: string | null = null;
    let lastTeam: string | null = null;
    const sync = () => {
      const raw = typeof window === "undefined" ? "[]" : localStorage.getItem(KEY) || "[]";
      if (raw !== lastRaw) {
        lastRaw = raw;
        setList(read());
      }
      const t = currentTeam();
      if (t !== lastTeam) {
        lastTeam = t;
        setTeam(t);
      }
      setNow(Date.now());
    };
    sync();
    window.addEventListener(EVT, sync);
    window.addEventListener("storage", sync);
    window.addEventListener("hive-team-changed", sync);
    let timer: ReturnType<typeof setInterval> | null = null;
    const active = () => typeof document === "undefined" || !document.hidden;
    const start = () => {
      if (!timer && active()) timer = setInterval(sync, 2000);
    };
    const onVisibility = () => {
      if (active()) {
        sync();
        start();
      } else if (timer) {
        clearInterval(timer);
        timer = null;
      }
    };
    start();
    if (typeof document !== "undefined")
      document.addEventListener("visibilitychange", onVisibility);
    return () => {
      if (timer) clearInterval(timer);
      if (typeof document !== "undefined")
        document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener(EVT, sync);
      window.removeEventListener("storage", sync);
      window.removeEventListener("hive-team-changed", sync);
    };
  }, []);
  return list.filter(
    (b) =>
      (!teamKnown(team) || !teamKnown(b.team) || b.team === team) &&
      (!opts?.project || b.project === opts.project) &&
      (!b.settledAt || now - b.settledAt < SETTLED_GRACE_MS),
  );
}

/** MERGE, never replace: drops pending rows whose REAL deployment record is already visible in the rendered list, keyed on the deployment id
 *  the build reported (never project+time: clock skew, concurrent redeploys). An unsettled row has no id and is never superseded; a settled row
 *  whose record never shows expires via `SETTLED_GRACE_MS`. */
export function mergePending<T extends { id: string }>(
  pending: PendingBuild[],
  deployments: T[] | null | undefined,
): PendingBuild[] {
  if (!deployments?.length) return pending;
  return pending.filter(
    (b) => !b.deploymentId || !deployments.some((d) => d.id === b.deploymentId),
  );
}
