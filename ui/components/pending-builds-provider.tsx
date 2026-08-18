"use client";

// App-wide background poller for in-flight deployments. Mounted once (in app-chrome)
// so pending builds are driven to completion regardless of which page is open — the
// "Building" rows the lists render come from the persistent store, so leaving and
// returning to a page (or reloading) keeps them visible until the build finishes.

import { useEffect } from "react";
import { apiGet, type Build } from "@/lib/api";
import { getPendingBuilds, removePendingBuild, markPendingSettled, rehomePendingTeam, SETTLED_GRACE_MS } from "@/lib/pending-builds";

const STALE_MS = 15 * 60_000; // a build that never resolves is pruned after 15 min

export function PendingBuildsProvider() {
  useEffect(() => {
    let stop = false;
    const tick = async () => {
      const list = getPendingBuilds();
      if (!list.length) return;
      // A row stored while identity was still resolving carries the "__pending__"
      // placeholder as its team, which would render for EVERY viewer. Re-home such
      // rows to the tenant the session actually resolved to.
      rehomePendingTeam();
      const now = Date.now();
      for (const b of list) {
        if (now - b.at > STALE_MS) {
          removePendingBuild(b.id);
          continue;
        }
        // Settled rows are reaped once the handover window closes, whether or not
        // any list ever observed the real record (a backgrounded tab renders none).
        if (b.settledAt && now - b.settledAt > SETTLED_GRACE_MS) {
          removePendingBuild(b.id);
          continue;
        }
        if (b.settledAt) continue; // already terminal; just waiting out the window
        try {
          const build = await apiGet<Build>(`/v1/builds/${b.id}`, { fresh: true });
          // Terminal → either the real deployment record now exists (ready), it
          // failed (error), or the user stopped it (cancelled, via the table's
          // own Cancel action or the /deploy/:id page's) — drop the pending row
          // in all three cases (the lists' own /deployments poll surfaces the
          // finished deployment when there is one).
          if (build.state === "ready") {
            // Do NOT drop the row here. The real deployment record is created on
            // whichever node ran the build, while the dashboard reads /deployments
            // through round-robin DNS — so for a beat after the build reports ready,
            // neither the pending row nor the real row exists on the node being
            // polled, and removing now is exactly what made the row blink out near
            // the end of a deploy. Mark it settled instead; the lists' `mergePending`
            // drops it the moment the real record is actually visible.
            markPendingSettled(b.id, build.deployment_id);
          } else if (build.state === "error" || build.state === "cancelled") {
            // No deployment record is coming, so there is nothing to hand over to.
            removePendingBuild(b.id);
          }
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
