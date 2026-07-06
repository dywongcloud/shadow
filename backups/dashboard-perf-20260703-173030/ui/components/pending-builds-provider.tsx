"use client";

// App-wide background poller for in-flight deployments. Mounted once (in app-chrome)
// so pending builds are driven to completion regardless of which page is open — the
// "Building" rows the lists render come from the persistent store, so leaving and
// returning to a page (or reloading) keeps them visible until the build finishes.

import { useEffect } from "react";
import { apiGet, type Build } from "@/lib/api";
import { getPendingBuilds, removePendingBuild } from "@/lib/pending-builds";

const STALE_MS = 15 * 60_000; // a build that never resolves is pruned after 15 min

export function PendingBuildsProvider() {
  useEffect(() => {
    let stop = false;
    const tick = async () => {
      const list = getPendingBuilds();
      if (!list.length) return;
      const now = Date.now();
      for (const b of list) {
        if (now - b.at > STALE_MS) {
          removePendingBuild(b.id);
          continue;
        }
        try {
          const build = await apiGet<Build>(`/v1/builds/${b.id}`, { fresh: true });
          // Terminal → the real deployment record now exists; drop the pending row
          // (the lists' own /deployments poll surfaces the finished deployment).
          if (build.state === "ready" || build.state === "error") removePendingBuild(b.id);
        } catch {
          /* build not visible yet (cross-node mirror lag) — keep polling */
        }
      }
    };
    // A backgrounded tab gains nothing from polling build status every 2s — pause
    // while hidden, and catch up with one immediate tick the moment it's visible
    // again instead of waiting out the rest of the interval.
    const active = () => typeof document === "undefined" || !document.hidden;
    tick();
    let t: ReturnType<typeof setInterval> | null = active() ? setInterval(() => { if (!stop) tick(); }, 2000) : null;
    const onVisibility = () => {
      if (stop) return;
      if (active()) {
        if (!t) {
          tick();
          t = setInterval(() => { if (!stop) tick(); }, 2000);
        }
      } else if (t) {
        clearInterval(t);
        t = null;
      }
    };
    if (typeof document !== "undefined") document.addEventListener("visibilitychange", onVisibility);
    return () => {
      stop = true;
      if (t) clearInterval(t);
      if (typeof document !== "undefined") document.removeEventListener("visibilitychange", onVisibility);
    };
  }, []);
  return null;
}
