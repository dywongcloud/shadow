"use client";

// App-wide background poller for in-flight deployments, mounted once (in app-chrome) so pending builds are driven to completion on whichever page is open; the
// "Building" rows the lists render come from the persistent store, so leaving or reloading a page keeps them visible until the build finishes.

import { useEffect } from "react";
import { apiGet, type Build } from "@/lib/api";
import { getPendingBuilds, removePendingBuild, markPendingSettled, rehomePendingTeam, SETTLED_GRACE_MS } from "@/lib/pending-builds";

const UNRESOLVED_BUILD_PRUNE_MS = 15 * 60_000;

export function PendingBuildsProvider() {
  useEffect(() => {
    let stop = false;
    const tick = async () => {
      const list = getPendingBuilds();
      if (!list.length) return;
      rehomePendingTeam();
      const now = Date.now();
      for (const b of list) {
        if (now - b.at > UNRESOLVED_BUILD_PRUNE_MS) {
          removePendingBuild(b.id);
          continue;
        }
        if (b.settledAt && now - b.settledAt > SETTLED_GRACE_MS) {
          removePendingBuild(b.id);
          continue;
        }
        if (b.settledAt) continue;
        try {
          const build = await apiGet<Build>(`/v1/builds/${b.id}`, { fresh: true });
          if (build.state === "ready") {
            markPendingSettled(b.id, build.deployment_id);
          } else if (build.state === "error" || build.state === "cancelled") {
            removePendingBuild(b.id);
          }
        } catch {}
      }
    };
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
