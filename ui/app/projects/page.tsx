"use client";

import Link from "next/link";
import { Search, GitBranch, Github } from "lucide-react";
import { useState } from "react";
import { Card, Button, Input, Triangle, Badge } from "@/components/ui";
import { ProjectMenu } from "@/components/project-menu";
import { usePoll, type Deployment } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function ProjectsPage() {
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  const [q, setQ] = useState("");
  const projects = new Map<string, Deployment>();
  for (const d of deps ?? []) if (!projects.has(d.project)) projects.set(d.project, d);
  const list = Array.from(projects.values()).filter((p) => p.project.toLowerCase().includes(q.toLowerCase()));

  return (
    <div>
      <div className="mb-6 flex items-center gap-2">
        <div className="relative flex-1">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <Input placeholder="Search Projects…" value={q} onChange={(e) => setQ(e.target.value)} className="pl-9" />
        </div>
        <Link href="/new"><Button>Add New…</Button></Link>
      </div>
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {list.map((p) => (
          <Link key={p.id} href={`/projects/${encodeURIComponent(p.project)}`}>
            <Card className="p-5 transition-shadow hover:shadow-pop">
              <div className="flex items-center gap-3">
                <Triangle />
                <div className="min-w-0">
                  <div className="truncate font-semibold">{p.project}</div>
                  <div className="truncate text-sm text-secondary">{p.alias}</div>
                </div>
                <Badge tone={p.state === "ready" ? "green" : p.state === "building" ? "amber" : "default"} className="ml-auto">
                  {p.state}
                </Badge>
                <ProjectMenu project={p.project} alias={p.alias} onChange={refresh} />
              </div>
              <p className="mt-4 line-clamp-1 text-sm text-secondary">{p.git?.commit_message || "—"}</p>
              <div className="mt-3 flex items-center gap-1.5 text-xs text-muted">
                {timeAgo(p.created_at_ms)} ago
                {p.git && <><Github className="ml-1 h-3.5 w-3.5" /><GitBranch className="h-3 w-3" />{p.git.branch}</>}
              </div>
            </Card>
          </Link>
        ))}
        {!list.length && <div className="text-sm text-secondary">No projects. <Link href="/new" className="text-link">Import one →</Link></div>}
      </div>
    </div>
  );
}
