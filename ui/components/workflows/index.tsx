"use client";

// ---------------------------------------------------------------------------
// The workflow observability console VIEWS — a faithful native port of Vercel's
// @workflow/web console (Home = Runs/Hooks/Workflows; Run detail lives in
// ../../app/workflows/runs/[id]). Rendered inside hive's own dashboard chrome
// ("minus the navbar"), wired to hive's /v1/workflows/* data plane.
// ---------------------------------------------------------------------------

import Link from "next/link";
import dynamic from "next/dynamic";
import { useEffect, useMemo, useState } from "react";
import { Loader2, Plus, Eye, EyeOff, GitBranch, X, ArrowDownUp, Workflow as WorkflowIcon, Webhook } from "lucide-react";
import { apiSend, usePoll, type WorkflowDef, type WorkflowRun, type WorkflowHook } from "@/lib/api";
import { Triangle } from "@/components/ui";
import { RunActions } from "@/components/workflows/run-actions";
import {
  CopyableText,
  EmptyState,
  FilterSelect,
  RelativeTime,
  SearchInput,
  StatusBadge,
  WfTableSkeleton,
  fmtDuration,
  isStaleRunning,
  normStatus,
  normalizeRun,
  parseFile,
  runDuration,
  statusMeta,
  useNow,
  TIME_RANGES,
  DEFAULT_RANGE,
  type WfStatus,
} from "./shared";

export { normalizeRun, fmtDuration, runDuration } from "./shared";

/** The three project-scoped workflow sub-views (mirrors the platform /workflows
 *  page), selected by the top-nav breadcrumb sub-tabs. */
export type WdkView = "runs" | "workflows" | "hooks";

// reactflow graph — heavy; only mounts when a workflow is opened.
const WorkflowDefGraph = dynamic(() => import("@/components/workflow-graph").then((m) => m.WorkflowDefGraph), {
  ssr: false,
  loading: () => (
    <div className="flex h-[420px] items-center justify-center rounded-xl border border-border bg-bg">
      <Loader2 className="h-5 w-5 animate-spin text-muted" />
    </div>
  ),
});

/* -------------------------------------------------------------------------- */
/* Status count chips                                                          */
/* -------------------------------------------------------------------------- */

const CHIP_ORDER: { key: WfStatus; label: string }[] = [
  { key: "completed", label: "Completed" },
  { key: "failed", label: "Failed" },
  { key: "running", label: "Running" },
  { key: "pending", label: "Pending" },
  { key: "cancelled", label: "Cancelled" },
];

export function StatusChips({ runs }: { runs: WorkflowRun[] }) {
  const counts = useMemo(() => {
    const c: Record<WfStatus, number> = { completed: 0, failed: 0, running: 0, pending: 0, cancelled: 0 };
    for (const r of runs) c[normStatus(r.status)]++;
    return c;
  }, [runs]);
  return (
    <div className="grid grid-cols-2 gap-2 sm:grid-cols-5">
      {CHIP_ORDER.map((c) => (
        <div key={c.key} className="flex items-center justify-between rounded-lg border border-border bg-card px-3 py-2.5">
          <span className="flex items-center gap-2 text-sm text-secondary">
            <span className={`h-2 w-2 rounded-full ${statusMeta(c.key).dot}`} /> {c.label}
          </span>
          <span className="text-base font-semibold tabular-nums">{counts[c.key]}</span>
        </div>
      ))}
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/* Runs table (faithful columns)                                              */
/* -------------------------------------------------------------------------- */

const COLS_PROJECT = "grid-cols-[minmax(0,1.4fr)_minmax(0,1fr)_minmax(0,1.6fr)_0.9fr_0.9fr_0.9fr_2.5rem]";
const COLS_NO_PROJECT = "grid-cols-[minmax(0,1.6fr)_minmax(0,1.8fr)_0.9fr_0.9fr_0.9fr_2.5rem]";

export function RunsTable({ runs, now, showProject }: { runs: WorkflowRun[]; now: number; showProject?: boolean }) {
  const grid = showProject ? COLS_PROJECT : COLS_NO_PROJECT;
  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      <div className={`grid ${grid} border-b border-border bg-subtle/30 px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted`}>
        <span>Workflow</span>
        {showProject && <span>Project</span>}
        <span>Run ID</span>
        <span>Status</span>
        <span>Duration</span>
        <span>Started</span>
        <span></span>
      </div>
      {runs.length === 0 ? (
        <EmptyState title="No runs" hint="Runs appear here as your deployed workflows execute." />
      ) : (
        runs.map((r) => {
          const live = normStatus(r.status) === "running";
          const stale = isStaleRunning(r, now);
          return (
            <Link
              key={r.id}
              href={`/workflows/runs/${encodeURIComponent(r.id)}${r.project ? `?project=${encodeURIComponent(r.project)}` : ""}`}
              className={`grid ${grid} items-center border-b border-border px-4 py-3 text-sm transition-colors last:border-0 hover:bg-subtle/50`}
            >
              <span className="truncate pr-2 font-medium">{r.name || r.def_id}</span>
              {showProject && (
                <span className="flex min-w-0 items-center gap-1.5 pr-2 text-secondary">
                  <Triangle className="h-4 w-4" /> <span className="truncate">{r.project || "default"}</span>
                </span>
              )}
              <span className="truncate pr-2 font-mono text-xs text-secondary">{r.id}</span>
              <StatusBadge status={r.status} live={live} />
              <span className="flex items-center gap-1.5 tabular-nums text-secondary">
                {fmtDuration(runDuration(r, now))}
                {live && !stale && <Loader2 className="h-3 w-3 animate-spin text-muted" />}
                {stale && (
                  <span className="rounded bg-amber-500/15 px-1 text-[10px] text-amber-600 dark:text-amber-400" title="Still marked running, but the run has produced no activity for hours — it likely stalled or its app was redeployed.">
                    stale
                  </span>
                )}
              </span>
              <span className="text-muted">
                <RelativeTime ms={r.started_ms} />
              </span>
              <span className="flex justify-end">
                {r.project ? (
                  <RunActions runId={r.id} project={r.project} status={r.status} hasPendingSleeps={live} variant="menu" />
                ) : null}
              </span>
            </Link>
          );
        })
      )}
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/* Runs LIST — filter bar + chips + table (shared by global + project)         */
/* -------------------------------------------------------------------------- */

const STATUS_FILTER_OPTS = [
  { value: "any", label: "Any status" },
  { value: "completed", label: "Completed" },
  { value: "running", label: "Running" },
  { value: "failed", label: "Failed" },
  { value: "pending", label: "Pending" },
  { value: "cancelled", label: "Cancelled" },
];

export function RunsList({
  runs,
  loading,
  showProject,
}: {
  runs: WorkflowRun[] | null;
  loading?: boolean;
  showProject?: boolean;
}) {
  const [q, setQ] = useState("");
  const [status, setStatus] = useState("any");
  const [workflow, setWorkflow] = useState("all");
  const [project, setProject] = useState("all");
  const [range, setRange] = useState(DEFAULT_RANGE);
  const [sortNewest, setSortNewest] = useState(true);

  const all = runs ?? [];
  const anyRunning = all.some((r) => normStatus(r.status) === "running" || normStatus(r.status) === "pending");
  const now = useNow(anyRunning ? 1000 : 5000);

  const workflowOpts = useMemo(() => {
    const names = Array.from(new Set(all.map((r) => r.name || r.def_id).filter(Boolean))).sort();
    return [{ value: "all", label: "All Workflows" }, ...names.map((n) => ({ value: n, label: n }))];
  }, [all]);
  const projectOpts = useMemo(() => {
    const ps = Array.from(new Set(all.map((r) => r.project).filter(Boolean))).sort();
    return [{ value: "all", label: "All Projects" }, ...ps.map((p) => ({ value: p, label: p }))];
  }, [all]);

  const filtered = useMemo(() => {
    const cutoff = now - (TIME_RANGES[range] ?? TIME_RANGES[DEFAULT_RANGE]);
    const needle = q.toLowerCase();
    const out = all.filter((r) => {
      if (isFinite(cutoff) && r.started_ms > 0 && r.started_ms < cutoff) return false;
      if (status !== "any" && normStatus(r.status) !== status) return false;
      if (workflow !== "all" && (r.name || r.def_id) !== workflow) return false;
      if (showProject && project !== "all" && r.project !== project) return false;
      if (needle && !(r.name.toLowerCase().includes(needle) || r.id.toLowerCase().includes(needle) || (r.project || "").toLowerCase().includes(needle))) return false;
      return true;
    });
    out.sort((a, b) => (sortNewest ? b.started_ms - a.started_ms : a.started_ms - b.started_ms));
    return out;
  }, [all, now, range, q, status, workflow, project, showProject, sortNewest]);

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <SearchInput value={q} onChange={setQ} placeholder="Search workflows, runs, and projects…" />
        <FilterSelect value={workflow} options={workflowOpts} onChange={setWorkflow} />
        <FilterSelect value={status} options={STATUS_FILTER_OPTS} onChange={setStatus} />
        {showProject && <FilterSelect value={project} options={projectOpts} onChange={setProject} />}
        <FilterSelect value={range} options={Object.keys(TIME_RANGES)} onChange={setRange} />
        <button
          type="button"
          onClick={() => setSortNewest((v) => !v)}
          className="flex items-center gap-1.5 rounded-md border border-border-strong px-3 py-1.5 text-sm text-fg hover:bg-subtle"
        >
          <ArrowDownUp className="h-3.5 w-3.5 text-muted" /> {sortNewest ? "Newest" : "Oldest"}
        </button>
      </div>

      <StatusChips runs={filtered} />

      {loading && !runs ? (
        <WfTableSkeleton rows={6} cols={showProject ? 6 : 5} />
      ) : all.length > 0 && filtered.length === 0 ? (
        <div className="overflow-hidden rounded-xl border border-border bg-card">
          <EmptyState title="No runs match these filters" hint="Try a wider time range or clear the status/workflow filter." />
        </div>
      ) : (
        <RunsTable runs={filtered} now={now} showProject={showProject} />
      )}
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/* Workflows list (definitions + graph sheet)                                  */
/* -------------------------------------------------------------------------- */

export function WorkflowsList({ defs }: { defs: WorkflowDef[] }) {
  const [openId, setOpenId] = useState<string | null>(null);
  const open = defs.find((d) => d.id === openId) || null;
  async function run(id: string) {
    await apiSend("POST", `/v1/workflows/${encodeURIComponent(id)}/run`).catch(() => {});
  }
  return (
    <div className="space-y-4">
      <div className="overflow-hidden rounded-xl border border-border bg-card">
        <div className="grid grid-cols-[minmax(0,1.4fr)_minmax(0,1.4fr)_0.7fr_auto] border-b border-border bg-subtle/30 px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
          <span>Workflow</span>
          <span>File</span>
          <span>Steps</span>
          <span></span>
        </div>
        {defs.length === 0 ? (
          <EmptyState
            title="No Workflows Found"
            hint="Deploy an app that uses the Vercel Workflow Development Kit — its workflows appear here automatically."
            icon={<WorkflowIcon className="h-6 w-6" />}
          />
        ) : (
          defs.map((d) => {
            const hasGraph = !!(d.graph && d.graph.nodes && d.graph.nodes.length);
            const stepCount = d.steps?.length || (hasGraph ? d.graph!.nodes.filter((n) => (n.data?.nodeKind || "").includes("step")).length : 0);
            return (
              <div key={d.id} className="grid grid-cols-[minmax(0,1.4fr)_minmax(0,1.4fr)_0.7fr_auto] items-center gap-3 border-b border-border px-4 py-3 text-sm last:border-0">
                <span className="truncate font-mono font-medium">{d.name}</span>
                <span className="truncate font-mono text-xs text-muted">{parseFile(d.id) || d.project || "—"}</span>
                <span>
                  <span className="inline-flex items-center rounded-full border border-border bg-subtle px-2 py-0.5 text-xs tabular-nums text-secondary">{stepCount}</span>
                </span>
                <div className="flex justify-end gap-2">
                  {hasGraph && (
                    <button
                      type="button"
                      onClick={() => setOpenId(openId === d.id ? null : d.id)}
                      className="flex items-center gap-1 rounded-md border border-border-strong px-2.5 py-1 text-xs text-fg hover:bg-subtle"
                    >
                      <GitBranch className="h-3.5 w-3.5" /> {openId === d.id ? "Hide" : "Graph"}
                    </button>
                  )}
                  <button type="button" onClick={() => run(d.id)} className="rounded-md border border-border-strong px-2.5 py-1 text-xs text-fg hover:bg-subtle">
                    Run
                  </button>
                </div>
              </div>
            );
          })
        )}
      </div>
      {open && open.graph && (
        <div className="rounded-xl border border-border bg-card p-3">
          <div className="mb-2 flex items-center justify-between">
            <div className="text-sm font-medium">{open.name} — graph</div>
            <button type="button" onClick={() => setOpenId(null)} className="text-muted hover:text-fg">
              <X className="h-4 w-4" />
            </button>
          </div>
          <WorkflowDefGraph def={open} />
        </div>
      )}
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/* Hooks table                                                                 */
/* -------------------------------------------------------------------------- */

export function HooksTable({ hooks, loading }: { hooks: WorkflowHook[] | null; loading?: boolean }) {
  if (loading && !hooks) return <WfTableSkeleton rows={4} cols={5} />;
  const rows = hooks ?? [];
  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      <div className="grid grid-cols-[minmax(0,1.3fr)_minmax(0,1.3fr)_minmax(0,1.2fr)_0.8fr_0.7fr] border-b border-border bg-subtle/30 px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
        <span>Hook ID</span>
        <span>Run ID</span>
        <span>Token</span>
        <span>Created</span>
        <span>Invocations</span>
      </div>
      {rows.length === 0 ? (
        <EmptyState
          title="No hooks"
          hint="Hooks appear here when a workflow calls createHook / exposes a webhook trigger."
          icon={<Webhook className="h-6 w-6" />}
        />
      ) : (
        rows.map((h) => (
          <div key={h.hookId} className="grid grid-cols-[minmax(0,1.3fr)_minmax(0,1.3fr)_minmax(0,1.2fr)_0.8fr_0.7fr] items-center gap-2 border-b border-border px-4 py-3 text-sm last:border-0">
            <CopyableText text={h.hookId} className="truncate text-xs text-secondary" truncate />
            <Link href={`/workflows/runs/${encodeURIComponent(h.runId)}${h.project ? `?project=${encodeURIComponent(h.project)}` : ""}`} className="truncate font-mono text-xs text-link hover:underline">
              {h.runId}
            </Link>
            <TokenCell token={h.token} />
            <span className="text-muted">
              <RelativeTime ms={h.created_ms} />
            </span>
            <span className="tabular-nums text-secondary">{h.invocations}</span>
          </div>
        ))
      )}
    </div>
  );
}

function TokenCell({ token }: { token?: string | null }) {
  const [show, setShow] = useState(false);
  if (!token) return <span className="text-muted">—</span>;
  return (
    <span className="flex min-w-0 items-center gap-1.5">
      {show ? (
        <CopyableText text={token} className="truncate text-xs text-secondary" truncate />
      ) : (
        <span className="font-mono text-xs text-muted">••••••••</span>
      )}
      <button type="button" onClick={() => setShow((v) => !v)} className="shrink-0 text-muted hover:text-fg" title={show ? "Hide" : "Reveal"}>
        {show ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
      </button>
    </span>
  );
}

/* -------------------------------------------------------------------------- */
/* Hooks (project or global) — self-fetching wrapper                           */
/* -------------------------------------------------------------------------- */

export function HooksView({ project, defs }: { project?: string; defs?: WorkflowDef[] }) {
  const path = `/v1/workflows/hooks${project ? `?project=${encodeURIComponent(project)}` : ""}`;
  const { data, loading, error } = usePoll<WorkflowHook[]>(path, 10000);
  const triggerDefs = (defs ?? []).filter((d) => !project || (d.project || "") === project);
  return (
    <div className="space-y-4">
      {/* A backend that predates the hooks endpoint (or a transient error) reads
          as "no hooks" rather than an eternal skeleton. */}
      <HooksTable hooks={data ?? (error ? [] : null)} loading={loading && !data && !error} />
      {triggerDefs.length > 0 && (
        <div>
          <div className="mb-2 text-sm font-medium">Trigger endpoints</div>
          <p className="mb-2 text-xs text-secondary">Start a run from anything — POST to a workflow’s trigger URL.</p>
          <div className="space-y-2">
            {triggerDefs.map((d) => (
              <div key={d.id} className="rounded-xl border border-border bg-card p-3">
                <div className="mb-1 font-mono text-sm font-medium">{d.name}</div>
                <code className="block overflow-x-auto rounded-md border border-border bg-subtle/50 px-3 py-2 font-mono text-xs text-secondary">
                  POST /cloud/v1/workflows/{d.id}/run
                </code>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/* Project-scoped console (Runs / Workflows / Hooks)                           */
/* -------------------------------------------------------------------------- */

export function ProjectWorkflows({ project, view = "runs" }: { project: string; view?: WdkView }) {
  const [anyRunning, setAnyRunning] = useState(false);
  const { data: rawRuns, loading: runsLoading } = usePoll<WorkflowRun[]>(
    `/v1/workflows/runs?project=${encodeURIComponent(project)}`,
    anyRunning ? 2500 : 10000,
    view === "runs",
  );
  const { data: rawDefs } = usePoll<WorkflowDef[]>("/v1/workflows", 15000);
  const runs = useMemo(() => (rawRuns ?? []).map(normalizeRun), [rawRuns]);
  useEffect(() => {
    setAnyRunning(runs.some((r) => normStatus(r.status) === "running" || normStatus(r.status) === "pending"));
  }, [runs]);
  const defs = useMemo(() => (rawDefs ?? []).filter((d) => (d.project || "") === project), [rawDefs, project]);

  if (view === "workflows") return <ProjectDefsView project={project} defs={defs} />;
  if (view === "hooks") return <HooksView project={project} defs={defs} />;
  return <RunsList runs={rawRuns ? runs : null} loading={runsLoading && !rawRuns} />;
}

/** Project-scoped Workflows sub-view: the faithful WorkflowsList plus hive's
 *  "New Workflow" definer (define + immediately run against this project). */
function ProjectDefsView({ project, defs }: { project: string; defs: WorkflowDef[] }) {
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("pipeline");
  const [deployment, setDeployment] = useState(project);
  const [steps, setSteps] = useState("/api/hello,/api/cached");
  const [busy, setBusy] = useState(false);

  async function defineAndRun() {
    if (!deployment.trim()) return;
    setBusy(true);
    try {
      const id = `wf-${name}-${Math.random().toString(36).slice(2, 8)}`;
      await apiSend("POST", "/v1/workflows", {
        id,
        name,
        project,
        steps: steps.split(",").map((p, i) => ({ name: `step${i + 1}`, deployment, path: p.trim() })),
      });
      await apiSend("POST", `/v1/workflows/${id}/run`);
      setCreating(false);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex justify-end">
        <button
          type="button"
          onClick={() => setCreating((v) => !v)}
          className="flex items-center gap-1.5 rounded-md border border-border-strong px-3 py-1.5 text-sm text-fg hover:bg-subtle"
        >
          {creating ? <X className="h-4 w-4" /> : <Plus className="h-4 w-4" />}
          {creating ? "Close" : "New Workflow"}
        </button>
      </div>
      {creating && (
        <div className="rounded-xl border border-border bg-card p-4">
          <div className="mb-3 text-sm font-medium">New workflow in {project}</div>
          <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="name" className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm" />
            <input value={deployment} onChange={(e) => setDeployment(e.target.value)} placeholder="deployment alias" className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm" />
            <input value={steps} onChange={(e) => setSteps(e.target.value)} placeholder="step paths, comma-separated" className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm" />
          </div>
          <div className="mt-3 flex gap-2">
            <button type="button" onClick={defineAndRun} disabled={busy || !deployment} className="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg hover:opacity-90 disabled:opacity-50">
              {busy ? "Starting…" : "Define & Run"}
            </button>
            <button type="button" onClick={() => setCreating(false)} className="rounded-md px-3 py-1.5 text-sm text-secondary hover:bg-subtle hover:text-fg">
              Cancel
            </button>
          </div>
        </div>
      )}
      <WorkflowsList defs={defs} />
    </div>
  );
}
