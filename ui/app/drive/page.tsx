"use client";

import { useState } from "react";
import { Card, PageHeader } from "@/components/ui";
import { useProjectNames } from "@/lib/projects";
import { ProjectDrive } from "./project-drive";

/** Team-level Drive tab: a file browser against `/v1/drive/:project/*` (drive_api.rs). Drive is per PROJECT, so the picker lists every project the tenant owns. */
export default function DrivePage() {
  const projects = useProjectNames();
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
