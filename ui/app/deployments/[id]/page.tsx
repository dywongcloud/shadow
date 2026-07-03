"use client";

import { Suspense, useMemo, useState } from "react";
import { useSearchParams } from "next/navigation";
import Link from "next/link";
import {
  RotateCcw, ArrowLeft, ExternalLink, Code2, Terminal, GitBranch, Clock,
  Check, CircleX, Loader2, TriangleAlert, Globe, User,
} from "lucide-react";
import { Button, Card, Badge } from "@/components/ui";
import { BuildLogs } from "@/components/build-logs";
import { ProjectWorkflows } from "@/components/workflows";
import { RedeployModal } from "@/components/redeploy-modal";
import { apiSend, usePoll, type Deployment, type Build } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { deploymentUrl, deploymentHost } from "@/lib/deploy-url";

// `https://github.com/owner/repo(.git)` (or scp-style) → `owner/repo` for display.
function ownerRepo(url: string): string {
  if (!url) return "";
  return url
    .replace(/^git@[^:]+:/, "")
    .replace(/^https?:\/\/[^/]+\//, "")
    .replace(/\.git$/, "")
    .replace(/^\/+|\/+$/g, "");
}

/** Pull a concise headline for a failed build from its log lines. */
function errorHeadline(build: Build | null): string {
  if (!build) return "Build failed";
  const lines = build.lines ?? [];
  const exited = [...lines].reverse().find((l) => /exited with|non-zero exit|command failed/i.test(l.line));
  if (exited) return exited.line.trim();
  const anyErr = [...lines].reverse().find((l) => /error|failed/i.test(l.line));
  return anyErr ? anyErr.line.trim() : "Build failed";
}

export default function DeploymentDetailPage({ params }: { params: { id: string } }) {
  // useSearchParams (the top-nav `?tab=` selector) must sit under a Suspense boundary.
  return (
    <Suspense fallback={<div className="mx-auto max-w-5xl pb-16" />}>
      <DeploymentDetail id={params.id} />
    </Suspense>
  );
}

function DeploymentDetail({ id }: { id: string }) {
  // Which scope-view tab is active (from the top nav): overview | logs | workflows.
  const searchParams = useSearchParams();
  const tab = searchParams.get("tab");
  const { data: deps, refresh } = usePoll<Deployment[]>("/deployments", 3000);
  // The build behind this deployment (logs/timing/error). 404s until one exists.
  const { data: build } = usePoll<Build>(`/v1/deployments/${encodeURIComponent(id)}/build`, 2000);
  const [redeploy, setRedeploy] = useState(false);
  const [busy, setBusy] = useState(false);

  const dep = useMemo(() => (deps ?? []).find((d) => d.id === id) ?? null, [deps, id]);

  // Prefer the deployment's live state; fall back to the build's while it's still
  // being placed (the deployment row appears only once it exists).
  const state = dep?.state ?? build?.state ?? "queued";
  const ready = state === "ready";
  const errored = state === "error";
  const building = state === "building" || state === "queued";
  const env = dep ? (dep.target || (dep.production ? "production" : "preview")) : "preview";
  const duration =
    build && build.finished_ms ? `${Math.max(1, Math.round((build.finished_ms - build.started_ms) / 1000))}s` : building ? "in progress" : "—";
  const created = dep?.created_at_ms ?? build?.started_ms ?? 0;
  const creator = dep?.creator || "—";
  // A preview opens at its own immutable URL; production at the production alias.
  const self = dep ? (dep.production ? dep.alias : dep.commit_alias || dep.branch_alias || dep.id_alias || dep.alias) : build?.alias;
  const url = self ? deploymentUrl(self, dep?.region_code) : "";
  const host = self ? deploymentHost(self, dep?.region_code) : "";
  const git = dep?.git ?? (build?.repo_url ? { repo_url: build.repo_url, branch: build.branch, commit: build.commit, commit_message: build.commit_message } : null);

  async function promote() {
    if (!dep) return;
    setBusy(true);
    try {
      await apiSend("POST", `/v1/deployments/${dep.id}/promote`);
      await refresh();
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="mx-auto max-w-5xl pb-16">
      <Link
        href={dep ? `/projects/${encodeURIComponent(dep.project)}` : "/deployments"}
        className="mb-5 inline-flex items-center gap-2 text-sm text-secondary hover:text-fg"
      >
        <ArrowLeft className="h-4 w-4" /> {dep ? dep.project : "Deployments"}
      </Link>

      {tab === "workflows" ? (
        // Workflows scope-tab: this deployment's project workflows + runs (reuses
        // the same component the project page uses).
        dep ? (
          <ProjectWorkflows project={dep.project} />
        ) : (
          <div className="text-sm text-secondary">Loading…</div>
        )
      ) : (
      <>
      <Card className="mb-6 p-6">
        <div className="mb-5 flex items-start justify-between gap-3">
          <div>
            <h1 className="text-lg font-semibold">Deployment Details</h1>
            <p className="mt-0.5 font-mono text-xs text-muted">{id}</p>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {dep && !dep.production && ready && (
              <Button variant="outline" onClick={promote} disabled={busy}>
                {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <RotateCcw className="h-4 w-4" />} Promote
              </Button>
            )}
            {git && (
              <Button onClick={() => setRedeploy(true)}>
                <RotateCcw className="h-4 w-4" /> Redeploy
              </Button>
            )}
          </div>
        </div>

        <div className="grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,340px)_1fr]">
          {/* Status banner box */}
          <div
            className={`rounded-lg border p-4 ${
              errored ? "border-red-500/50 bg-red-500/5" : ready ? "border-green/40 bg-green/5" : "border-border bg-subtle/40"
            }`}
          >
            <div className="flex items-center gap-2 text-sm font-semibold">
              {ready ? (
                <Check className="h-4 w-4 text-green" />
              ) : errored ? (
                <TriangleAlert className="h-4 w-4 text-red-500" />
              ) : (
                <Loader2 className="h-4 w-4 animate-spin text-secondary" />
              )}
              {ready ? "Ready" : errored ? "Build Failed" : "Building"}
            </div>
            <p className="mt-2 min-h-[3rem] text-sm text-secondary">
              {errored ? (
                <span className="font-mono text-xs text-red-500">{errorHeadline(build)}</span>
              ) : ready ? (
                "Your deployment is live and serving traffic."
              ) : (
                "Your deployment is building…"
              )}
            </p>
          </div>

          {/* Meta grid */}
          <div className="grid grid-cols-2 gap-x-6 gap-y-5 sm:grid-cols-2">
            <Meta label="Created" icon={<User className="h-3.5 w-3.5" />}>
              <span className="font-medium">{creator}</span>
              {created ? <span className="text-secondary"> · {timeAgo(created)}</span> : null}
            </Meta>
            <Meta label="Status">
              <span className="inline-flex items-center gap-1.5">
                <span className={`h-2 w-2 rounded-full ${ready ? "bg-green" : errored ? "bg-red-500" : "bg-amber-400"}`} />
                {ready ? "Ready" : errored ? "Error" : building ? "Building" : state}
              </span>
            </Meta>
            <Meta label="Duration" icon={<Clock className="h-3.5 w-3.5" />}>
              {duration}
            </Meta>
            <Meta label="Environment" icon={<Globe className="h-3.5 w-3.5" />}>
              {env === "production" ? <Badge tone="green">Production</Badge> : <Badge>Preview</Badge>}
            </Meta>

            <div className="col-span-2">
              <MetaLabel>Domains</MetaLabel>
              {host ? (
                <a
                  href={url}
                  target="_blank"
                  rel="noreferrer"
                  className={`inline-flex items-center gap-1.5 text-sm ${ready ? "text-link hover:underline" : "text-muted"}`}
                >
                  <Globe className="h-3.5 w-3.5 shrink-0" />
                  {host}
                  {ready && <ExternalLink className="h-3 w-3" />}
                </a>
              ) : (
                <span className="text-sm text-muted">No domain yet</span>
              )}
            </div>

            <div className="col-span-2">
              <MetaLabel>Source</MetaLabel>
              {git ? (
                <div className="flex flex-col gap-1 text-sm">
                  <a
                    href={git.repo_url}
                    target="_blank"
                    rel="noreferrer"
                    className="inline-flex w-fit items-center gap-1.5 text-link hover:underline"
                  >
                    <Code2 className="h-3.5 w-3.5" /> {ownerRepo(git.repo_url) || "View code"}
                  </a>
                  <span className="inline-flex items-center gap-1.5 text-secondary">
                    <GitBranch className="h-3.5 w-3.5 shrink-0" />
                    <span className="font-mono text-xs">{git.branch || "main"}</span>
                    {git.commit && <span className="font-mono text-xs text-muted">{git.commit.slice(0, 7)}</span>}
                    {git.commit_message && <span className="truncate">{git.commit_message}</span>}
                  </span>
                </div>
              ) : (
                <span className="inline-flex items-center gap-1.5 text-sm text-secondary">
                  <Terminal className="h-3.5 w-3.5" /> CLI / upload deployment
                </span>
              )}
            </div>
          </div>
        </div>
      </Card>

      <BuildLogs build={build ?? null} />

      {!build && (
        <p className="mt-3 text-center text-xs text-muted">
          No build record found for this deployment on this node.
        </p>
      )}
      </>
      )}

      {redeploy && dep && (
        <RedeployModal
          deployment={dep}
          prodAlias={`${dep.project}.localhost`}
          onClose={() => setRedeploy(false)}
          onDone={() => refresh()}
        />
      )}
    </div>
  );
}

function MetaLabel({ children }: { children: React.ReactNode }) {
  return <div className="mb-1 text-xs font-medium text-muted">{children}</div>;
}

function Meta({ label, icon, children }: { label: string; icon?: React.ReactNode; children: React.ReactNode }) {
  return (
    <div>
      <MetaLabel>{label}</MetaLabel>
      <div className="flex items-center gap-1.5 text-sm">
        {icon && <span className="text-muted">{icon}</span>}
        <span>{children}</span>
      </div>
    </div>
  );
}
