"use client";

import { useMemo, useState } from "react";
import { Card, PageHeader } from "@/components/ui";
import { usePoll, type Deployment } from "@/lib/api";
import { ProjectDrive } from "./project-drive";

/**
 * Team-level Drive tab — a real file browser against `/v1/drive/:project/*`
 * (drive_api.rs). Drive is per-PROJECT, and there's no dedicated project-list
 * endpoint anywhere in this dashboard (`storage/page.tsx`'s `useDatabases`
 * hits the exact same gap) — so the picker is derived from `/deployments`
 * the same way, deliberately matching that precedent rather than inventing a
 * second convention. A project with no deployment yet won't appear here
 * until it has one, same limitation the Storage page already has.
 */
export default function DrivePage() {
  const { data: deps } = usePoll<Deployment[]>("/deployments", 15000);
  const projects = useMemo(
    () => Array.from(new Set((deps ?? []).map((d) => d.project))).sort(),
    [deps]
  );
  // No effect-driven default: derive the selected project at render time
  // (the `storage/page.tsx` `sqliteProject || projects[0] || ""` precedent)
  // so there's no synchronous setState-in-effect render cascade.
  const [projectOverride, setProjectOverride] = useState("");
  const project = projectOverride || projects[0] || "";

  return (
    <div>
      <PageHeader
        title="Drive"
        desc="A real, per-project file store — browse, upload, organize, and mount it as a network drive over WebDAV."
        action={
          projects.length > 0 ? (
            <select
              value={project}
              onChange={(e) => setProjectOverride(e.target.value)}
              className="rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none focus:ring-1 focus:ring-link"
            >
              {projects.map((p) => (
                <option key={p} value={p}>
                  {p}
                </option>
              ))}
            </select>
          ) : undefined
        }
      />
      {projects.length === 0 ? (
        <Card className="py-12 text-center text-sm text-secondary">
          No projects yet — deploy a project first, then its Drive appears here.
        </Card>
      ) : project ? (
        <ProjectDrive key={project} project={project} />
      ) : null}
    </div>
  );
}
