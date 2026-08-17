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
  /**
   * Epoch ms the build reached a terminal READY state, if it has. The row is
   * deliberately kept for a grace window afterwards: the real deployment record
   * is created on the node that ran the build, and the dashboard reads
   * `/deployments` through round-robin DNS, so for a beat after the build
   * finishes NEITHER the pending row nor the real row exists on the node being
   * polled. Removing on `ready` is what made the row blink out near the end of a
   * deploy and reappear moments later.
   */
  settledAt?: number;
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

/**
 * Mark a build as finished-and-ready without removing its row yet. The row keeps
 * rendering (as "Finalizing") until either the real deployment record shows up in
 * the list being rendered, or `SETTLED_GRACE_MS` elapses.
 */
export function markPendingSettled(id: string): void {
  const list = read();
  let changed = false;
  const next = list.map((x) => {
    if (x.id !== id || x.settledAt) return x;
    changed = true;
    return { ...x, settledAt: Date.now() };
  });
  if (changed) write(next);
}

/** Drop a pending build (build finished, or being pruned). */
export function removePendingBuild(id: string): void {
  const list = read();
  const next = list.filter((x) => x.id !== id);
  if (next.length !== list.length) write(next);
}

/**
 * Is this tenant value actually KNOWN? `currentTeam()` returns the `__pending__`
 * placeholder (and, before hydration, an empty string) while identity sync is still
 * resolving `hive_uid` / `hive_is_owner` — see `lib/api.ts`. An unresolved value
 * means "not known yet", never "does not match", and treating the two the same is
 * what made a deploying user's own row vanish mid-build on any reload: the row was
 * stored under the resolved tenant at click time, then compared against
 * `__pending__` on the next page load and filtered straight out.
 */
function teamKnown(t: string): boolean {
  return !!t && t !== "__pending__";
}

/** How long a finished (ready) row keeps rendering while the real record propagates. */
export const SETTLED_GRACE_MS = 90_000;

/**
 * React hook: the current team's pending builds (optionally scoped to one project),
 * live-updated on same-tab changes, cross-tab `storage` events, and team switches.
 *
 * Never hides a row merely because the current tenant is still resolving — an
 * in-flight build the user started must stay visible for its whole lifecycle.
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
    // `hive_uid` / `hive_is_owner` are written by the topnav's identity effects, which
    // only re-broadcast `hive-team-changed` when the resolved value CHANGES — so the
    // very first resolution of a fresh session lands with no event at all and this
    // hook would keep comparing against a stale `__pending__`. A cheap poll closes
    // that window without coupling this store to the identity plumbing.
    const t = setInterval(sync, 2000);
    return () => {
      clearInterval(t);
      window.removeEventListener(EVT, sync);
      window.removeEventListener("storage", sync);
      window.removeEventListener("hive-team-changed", sync);
    };
  }, []);
  const now = Date.now();
  return list.filter(
    (b) =>
      // Tenant filter applies only when BOTH sides are known.
      (!teamKnown(team) || !teamKnown(b.team) || b.team === team) &&
      (!opts?.project || b.project === opts.project) &&
      // A settled row lingers only for the propagation window.
      (!b.settledAt || now - b.settledAt < SETTLED_GRACE_MS),
  );
}

/**
 * MERGE, never replace: drop the pending rows a real deployment record already
 * covers, so a list can render both sources without the row either duplicating or
 * blinking out in the handover. A pending row is superseded once a deployment for
 * the same project exists that was created at/after the build started.
 */
export function mergePending<T extends { project: string; created_at_ms: number }>(
  pending: PendingBuild[],
  deployments: T[] | null | undefined,
): PendingBuild[] {
  if (!deployments?.length) return pending;
  return pending.filter(
    (b) => !deployments.some((d) => d.project === b.project && d.created_at_ms >= b.at),
  );
}
