"use client";

// Persistent store for IN-FLIGHT deployments (builds that haven't produced a real
// deployment record yet). Held in localStorage so a "Building" row survives
// navigation AND a full page reload — the bug being fixed was that this state lived
// in component-local `useState` and vanished the moment you left the page.
//
// Each entry is a tiny state machine: it enters on deploy (queued/building) and is
// removed by the global poller (`PendingBuildsProvider`) once its build reaches a
// terminal state (ready/error) — at which point the real deployment row appears
// from `/deployments`. A stale-prune backstops any build that never resolves.

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
}

const KEY = "hive_pending_builds";
const EVT = "hive-pending-builds"; // same-tab change notification

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

/** Drop a pending build (build finished, or being pruned). */
export function removePendingBuild(id: string): void {
  const list = read();
  const next = list.filter((x) => x.id !== id);
  if (next.length !== list.length) write(next);
}

/**
 * React hook: the current team's pending builds (optionally scoped to one project),
 * live-updated on same-tab changes, cross-tab `storage` events, and team switches.
 */
export function usePendingBuilds(opts?: { project?: string }): PendingBuild[] {
  const [list, setList] = useState<PendingBuild[]>([]);
  const [team, setTeam] = useState<string>("");
  useEffect(() => {
    const sync = () => {
      setList(read());
      setTeam(currentTeam());
    };
    sync();
    window.addEventListener(EVT, sync);
    window.addEventListener("storage", sync);
    window.addEventListener("hive-team-changed", sync);
    return () => {
      window.removeEventListener(EVT, sync);
      window.removeEventListener("storage", sync);
      window.removeEventListener("hive-team-changed", sync);
    };
  }, []);
  return list.filter((b) => b.team === team && (!opts?.project || b.project === opts.project));
}
