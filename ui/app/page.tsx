"use client";

import Link from "next/link";
import { Github, Search, Plus, GitBranch } from "lucide-react";
import { Card, Button, Input, Triangle, Badge } from "@/components/ui";
import { GlobeEmptyState } from "@/components/globe";
import { usePoll, type Deployment, type Event, type Overview } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { useState } from "react";

export default function OverviewPage() {
  const { data: deps } = usePoll<Deployment[]>("/deployments", 3000);
  const { data: events } = usePoll<Event[]>("/v1/logs?limit=8", 2500);
  const { data: ov } = usePoll<Overview>("/v1/overview", 4000);
  const [q, setQ] = useState("");

  // Group deployments by project (latest per project = the project card).
  const projects = new Map<string, Deployment>();
  for (const d of deps ?? []) if (!projects.has(d.project)) projects.set(d.project, d);
  const list = Array.from(projects.values()).filter((p) =>
    p.project.toLowerCase().includes(q.toLowerCase())
  );

  return (
    <div>
      {/* Header row */}
      <div className="mb-8 flex items-start justify-between">
        <div className="flex items-center gap-4">
          <div className="flex h-14 w-14 items-center justify-center rounded-full bg-[#0761d1] text-lg font-semibold text-white">
            D
          </div>
          <div>
            <h1 className="text-2xl font-semibold tracking-tight">Dylan&apos;s Projects</h1>
            <div className="mt-1 flex items-center gap-1.5 text-sm text-secondary">
              <Github className="h-4 w-4" />
              Connected to GitHub
              <span className="text-muted">/</span>
              <Link href="/integrations" className="text-link hover:underline">
                Settings
              </Link>
            </div>
          </div>
        </div>
      </div>

      <div className="grid grid-cols-1 gap-6 lg:grid-cols-[1fr_320px]">
        {/* Projects */}
        <div>
          <div className="mb-4 flex items-center gap-2">
            <div className="relative flex-1">
              <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
              <Input
                placeholder="Search…"
                value={q}
                onChange={(e) => setQ(e.target.value)}
                className="pl-9"
              />
            </div>
            <Link href="/new">
              <Button>New Project</Button>
            </Link>
          </div>

          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            {list.map((p) => (
              <Link key={p.id} href={`/projects/${encodeURIComponent(p.project)}`}>
                <Card className="p-5 transition-shadow hover:shadow-pop">
                  <div className="flex items-center gap-3">
                    <Triangle />
                    <div className="min-w-0">
                      <div className="flex items-center gap-2">
                        <span className="truncate font-semibold">{p.project}</span>
                        <span className="rounded-full border border-emerald-300 px-1.5 text-[11px] font-medium text-emerald-600">
                          100
                        </span>
                      </div>
                      <div className="truncate text-sm text-secondary">{p.alias}</div>
                    </div>
                  </div>
                  <p className="mt-4 line-clamp-2 text-sm text-secondary">
                    {p.git?.commit_message || "Deployed via dashboard"}
                  </p>
                  <div className="mt-4 flex items-center gap-1.5 text-xs text-muted">
                    {timeAgo(p.created_at_ms)} ago via{" "}
                    {p.git ? <Github className="h-3.5 w-3.5" /> : <span>CLI</span>}
                    {p.git?.branch ? (
                      <span className="ml-1 inline-flex items-center gap-1">
                        <GitBranch className="h-3 w-3" />
                        {p.git.branch}
                      </span>
                    ) : null}
                  </div>
                </Card>
              </Link>
            ))}
            {!list.length && (
              <Card className="col-span-full overflow-hidden p-8 text-center">
                <GlobeEmptyState title="Deploy your first project" desc="Import a Git repository or a Dockerfile to deploy across your global mesh." />
                <div className="relative z-10 -mt-24 flex justify-center">
                  <Link href="/new">
                    <Button>
                      <Plus className="h-4 w-4" /> New Project
                    </Button>
                  </Link>
                </div>
              </Card>
            )}
          </div>
        </div>

        {/* Recent Activity */}
        <Card className="h-fit p-5">
          <div className="mb-4 text-sm font-semibold">Recent Activity</div>
          <div className="flex flex-col gap-4">
            {(events ?? []).map((e, i) => (
              <div key={i} className="flex items-start gap-3 text-sm">
                <Triangle className="h-7 w-7 shrink-0" />
                <div className="min-w-0 flex-1">
                  <span className="text-secondary">
                    <span className="font-medium text-fg">node-{ov?.region ?? "a"}</span> {e.action}{" "}
                    <span className="font-medium text-fg">{e.path}</span>
                  </span>
                  <div className="text-xs text-muted">{e.status || ""} · {e.host}</div>
                </div>
                <span className="shrink-0 text-xs text-muted">{timeAgo(e.ts_ms)}</span>
              </div>
            ))}
            {!events?.length && <div className="text-sm text-secondary">No activity yet.</div>}
          </div>
        </Card>
      </div>
    </div>
  );
}
