"use client";

import Link from "next/link";
import { Github, Search, GitBranch, CheckCheck } from "lucide-react";
import { Card, Button, Input, Triangle, Badge } from "@/components/ui";
import { GlobeEmptyState } from "@/components/globe";
import { ProjectMenu } from "@/components/project-menu";
import { WithIdentity } from "@/components/identity";
import { usePoll, type Deployment, type Event, type Overview } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { useState } from "react";

export default function OverviewPage() {
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  const { data: events } = usePoll<Event[]>("/v1/logs?limit=8", 2500);
  const { data: ov } = usePoll<Overview>("/v1/overview", 4000);
  const [q, setQ] = useState("");
  const [page, setPage] = useState(0);

  // Group deployments by project (latest per project = the project card).
  const projects = new Map<string, Deployment>();
  for (const d of deps ?? []) if (!projects.has(d.project)) projects.set(d.project, d);
  const list = Array.from(projects.values()).filter((p) =>
    p.project.toLowerCase().includes(q.toLowerCase())
  );

  // Paginate so large fleets stay navigable.
  const PAGE = 6;
  const pageCount = Math.max(1, Math.ceil(list.length / PAGE));
  const safePage = Math.min(page, pageCount - 1);
  const shown = list.slice(safePage * PAGE, safePage * PAGE + PAGE);

  return (
    <div className="pb-24">
      {/* Header row */}
      <WithIdentity>
        {(id) => (
          <div className="mb-8 flex items-start justify-between">
            <div className="flex items-center gap-4">
              {id.imageUrl ? (
                // eslint-disable-next-line @next/next/no-img-element
                <img src={id.imageUrl} alt="" className="h-14 w-14 rounded-full object-cover" />
              ) : (
                <div className="flex h-14 w-14 items-center justify-center rounded-full bg-[#0761d1] text-lg font-semibold text-white">
                  {id.initial}
                </div>
              )}
              <div>
                <h1 className="text-2xl font-semibold tracking-tight">{id.name}&apos;s Projects</h1>
                <div className="mt-1 flex items-center gap-1.5 text-sm text-secondary">
                  <Github className="h-4 w-4" />
                  {id.email || "Connected to GitHub"}
                  <span className="text-muted">/</span>
                  <Link href="/integrations" className="text-link hover:underline">
                    Settings
                  </Link>
                </div>
              </div>
            </div>
          </div>
        )}
      </WithIdentity>

      <div className="grid grid-cols-1 gap-6 lg:grid-cols-[1fr_320px]">
        {/* Projects */}
        <div>
          <div className="mb-4 flex items-center gap-2">
            <div className="relative flex-1">
              <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
              <Input
                placeholder="Search…"
                value={q}
                onChange={(e) => { setQ(e.target.value); setPage(0); }}
                className="pl-9"
              />
            </div>
            <Link href="/new">
              <Button>New Project</Button>
            </Link>
          </div>

          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            {shown.map((p) => (
              <ProjectCard key={p.id} p={p} onChange={refresh} />
            ))}
            {!list.length && (
              <Card className="col-span-full overflow-hidden p-8 text-center">
                <GlobeEmptyState title="Deploy your first project" desc="Import a Git repository or a Dockerfile to deploy across your global mesh. Use “New Project” above to get started." />
              </Card>
            )}
          </div>

          {pageCount > 1 && (
            <div className="mt-5 flex items-center justify-between text-sm">
              <span className="text-muted">
                {list.length} project{list.length === 1 ? "" : "s"} · page {safePage + 1} of {pageCount}
              </span>
              <div className="flex gap-2">
                <Button variant="outline" disabled={safePage === 0} onClick={() => setPage(safePage - 1)}>Previous</Button>
                <Button variant="outline" disabled={safePage >= pageCount - 1} onClick={() => setPage(safePage + 1)}>Next</Button>
              </div>
            </div>
          )}
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

function repoLabel(url: string): string {
  return url
    .replace(/^https?:\/\/(www\.)?github\.com\//, "")
    .replace(/^https?:\/\//, "")
    .replace(/\.git$/, "");
}

/** Circular deployment-status ring with a double-check, like Vercel's. */
function StatusRing({ state }: { state: string }) {
  const tone =
    state === "ready" ? "text-green border-green/40"
    : state === "building" ? "text-amber-500 border-amber-500/40"
    : state === "error" ? "text-red-500 border-red-500/40"
    : "text-muted border-border-strong";
  return (
    <span className={`flex h-8 w-8 items-center justify-center rounded-full border-2 ${tone}`}>
      <CheckCheck className="h-3.5 w-3.5" />
    </span>
  );
}

function ProjectCard({ p, onChange }: { p: Deployment; onChange?: () => void }) {
  const hasGit = !!p.git;
  return (
    <Card className="p-5 transition-shadow hover:shadow-pop">
      <div className="flex items-start justify-between gap-2">
        <Link href={`/projects/${encodeURIComponent(p.project)}`} className="flex min-w-0 items-center gap-3">
          <Triangle />
          <div className="min-w-0">
            <div className="truncate font-semibold">{p.project}</div>
            <div className="truncate text-sm text-secondary">{p.alias}</div>
          </div>
        </Link>
        <div className="flex shrink-0 items-center gap-1">
          <StatusRing state={p.state} />
          <ProjectMenu project={p.project} alias={p.alias} onChange={onChange} />
        </div>
      </div>

      {hasGit ? (
        <>
          <div className="mt-3">
            <span className="inline-flex max-w-full items-center gap-1.5 truncate rounded-md bg-subtle px-2 py-1 text-xs text-secondary">
              <Github className="h-3.5 w-3.5 shrink-0" />
              <span className="truncate">{repoLabel(p.git!.repo_url)}</span>
            </span>
          </div>
          <p className="mt-3 truncate text-sm">{p.git!.commit_message || "Latest deployment"}</p>
          <div className="mt-3 flex items-center gap-1.5 text-xs text-muted">
            {timeAgo(p.created_at_ms)} ago on
            <GitBranch className="h-3 w-3" />
            {p.git!.branch || "main"}
          </div>
        </>
      ) : (
        <>
          <Link href="/new" className="mt-3 inline-block text-sm font-medium text-link hover:underline">
            Connect Git Repository
          </Link>
          <div className="mt-3 text-xs text-muted">{timeAgo(p.created_at_ms)} ago</div>
        </>
      )}
    </Card>
  );
}
