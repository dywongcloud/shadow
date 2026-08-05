"use client";

import Link from "next/link";
import { ExternalLink, Radio } from "lucide-react";
import { useRunNode } from "@/lib/use-run-node";
import { dbLaneStateLabel, type DbLaneStatus, type RunNodeStatus } from "@/lib/run-node-status";

/**
 * bn-storages-page-browser-db-wiring: the SAME db-lane status the /run-node
 * page renders (`dbStatus`, `DbLaneStatus`), relocated to the Storages page
 * context — read-only, exactly like `RunNodeControl`'s navbar precedent
 * (`components/run-node-control.tsx`): this component never starts/stops
 * anything itself, it just attaches to the one per-origin SharedWorker (if
 * this browser is already running one) and shows what it's doing for THIS
 * project. Starting/stopping a node — the consent flow, target picker,
 * geolocation — stays on the dedicated /run-node page; duplicating that flow
 * here would fork the one place it was carefully made hydration-safe.
 */
export function BrowserDbLiveStatus({ project }: { project: string }) {
  const { status, supported } = useRunNode();
  const db = (status as RunNodeStatus & { db?: DbLaneStatus | null }).db ?? null;

  if (!supported) {
    return (
      <p className="text-xs text-muted">
        This browser can&apos;t run a background node (no SharedWorker support), so it can&apos;t preview live sync here.
      </p>
    );
  }

  if (!db || db.project !== project) {
    return (
      <p className="flex items-center gap-1.5 text-xs text-secondary">
        <Radio className="h-3.5 w-3.5 text-muted" />
        Not replicating in this browser right now.{" "}
        <Link href="/run-node" className="inline-flex items-center gap-1 text-link hover:underline">
          Run a node <ExternalLink className="h-3 w-3" />
        </Link>{" "}
        against this project to preview live sync from here.
      </p>
    );
  }

  return (
    <div className="flex flex-col gap-1 rounded-lg border border-border bg-subtle p-3 text-xs text-secondary">
      <div className="flex items-center justify-between">
        <span className="font-medium text-fg">This browser&apos;s replica</span>
        <Link href="/run-node" className="inline-flex items-center gap-1 text-link hover:underline">
          Manage <ExternalLink className="h-3 w-3" />
        </Link>
      </div>
      <div>
        {db.access === "read_write" ? "read-write" : "read-only"} · {dbLaneStateLabel(db)}
        {db.state !== "error" && db.state !== "opening" && (
          <>
            {" "}· v{db.siteVersion} across {db.sites} site{db.sites === 1 ? "" : "s"}, {db.peers} sync peer{db.peers === 1 ? "" : "s"}
          </>
        )}
      </div>
      {db.error && <div className="text-red-500">{db.error}</div>}
    </div>
  );
}
