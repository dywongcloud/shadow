"use client";

import Link from "next/link";
import { useEffect, useMemo, useState } from "react";
import { Loader2, Search, Radio, Plus, X } from "lucide-react";
import { timeAgo } from "@/lib/utils";
import { apiSend, usePoll, type RunStatus, type WorkflowRun, type WorkflowDef } from "@/lib/api";
import { Button, Input } from "@/components/ui";

/** The three project-scoped workflow sub-views (mirrors the platform /workflows
 *  page), selected by the top-nav breadcrumb sub-tabs. */
export type WdkView = "runs" | "workflows" | "hooks";

/* ---- status mapping (Vercel vocabulary) ---- */
export interface StatusView { label: string; dot: string; text: string }
export function statusView(s: RunStatus): StatusView {
  switch (s) {
    case "succeeded": return { label: "Completed", dot: "bg-emerald-500", text: "text-emerald-600 dark:text-emerald-400" };
    case "failed": return { label: "Errored", dot: "bg-red-500", text: "text-red-600 dark:text-red-400" };
    case "running": return { label: "Active", dot: "bg-amber-500", text: "text-amber-600 dark:text-amber-400" };
    case "pending": return { label: "Pending", dot: "bg-zinc-400", text: "text-secondary" };
    case "cancelled": return { label: "Cancelled", dot: "bg-zinc-400", text: "text-secondary" };
  }
}

export function fmtDuration(ms: number): string {
  if (ms < 0) ms = 0;
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const rem = s % 60;
  if (m < 60) return `${m}m ${rem}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

export function runDuration(r: WorkflowRun, now: number): number {
  const end = r.finished_ms ?? now;
  return end - r.started_ms;
}

/* ---- WDK ("world") run normalization ----
 * Runs read from a deployed app's WDK world (e.g. @open-workflow/world-redis)
 * use a different shape (runId / workflowName / status: completed / epoch-seconds
 * timestamps). Map them onto our internal WorkflowRun so the existing table +
 * chips render both our-engine runs and real app runs uniformly. */
function toMs(t: unknown): number | null {
  if (typeof t !== "number") return null;
  return t > 1e12 ? t : t * 1000;
}
function cleanWfName(n?: string): string {
  if (!n) return "workflow";
  const parts = n.split("/").filter(Boolean);
  return parts[parts.length - 1] || n;
}
function mapWdkStatus(s?: string): RunStatus {
  switch ((s || "").toLowerCase()) {
    case "completed":
    case "succeeded": return "succeeded";
    case "running": return "running";
    case "failed":
    case "error": return "failed";
    case "cancelled": return "cancelled";
    default: return "pending";
  }
}
export function normalizeRun(r: any): WorkflowRun {
  const wdk = r.runId !== undefined || r.workflowName !== undefined || r.startedAt !== undefined;
  if (!wdk) return r as WorkflowRun;
  return {
    id: r.runId ?? r.id ?? "",
    def_id: r.workflowName ?? r.def_id ?? "",
    name: cleanWfName(r.workflowName ?? r.name),
    project: r.project ?? "",
    status: mapWdkStatus(r.status),
    steps: Array.isArray(r.steps) ? r.steps : [],
    started_ms: toMs(r.startedAt) ?? toMs(r.createdAt) ?? r.started_ms ?? 0,
    finished_ms: toMs(r.completedAt) ?? r.finished_ms ?? null,
    // Workflow runs execute on the deployed app → default to production. (The WDK
    // world store is shared across a project's environments and doesn't tag runs.)
    environment: r.environment ?? "production",
  };
}

/* ---- status count chips ---- */
const CHIP_ORDER: { key: RunStatus | "active"; label: string; dot: string }[] = [
  { key: "succeeded", label: "Completed", dot: "bg-emerald-500" },
  { key: "failed", label: "Errored", dot: "bg-red-500" },
  { key: "active", label: "Active", dot: "bg-amber-500" },
  { key: "pending", label: "Pending", dot: "bg-zinc-400" },
  { key: "cancelled", label: "Cancelled", dot: "bg-zinc-400" },
];

export function StatusChips({ runs }: { runs: WorkflowRun[] }) {
  const count = (k: RunStatus | "active") =>
    k === "active"
      ? runs.filter((r) => r.status === "running").length
      : runs.filter((r) => r.status === k).length;
  return (
    <div className="grid grid-cols-2 gap-2 sm:grid-cols-5">
      {CHIP_ORDER.map((c) => (
        <div key={c.key} className="flex items-center justify-between rounded-lg border border-border bg-card px-3 py-2.5">
          <span className="flex items-center gap-2 text-sm text-secondary">
            <span className={`h-2 w-2 rounded-full ${c.dot}`} /> {c.label}
          </span>
          <span className="text-base font-semibold tabular-nums">{count(c.key)}</span>
        </div>
      ))}
    </div>
  );
}

/* ---- runs table ---- */
export function RunsTable({ runs, now, showProject }: { runs: WorkflowRun[]; now: number; showProject?: boolean }) {
  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      <div className={`grid ${showProject ? "grid-cols-[1.4fr_1.6fr_0.9fr_0.8fr_0.9fr]" : "grid-cols-[1.4fr_1.8fr_0.9fr_0.8fr_0.9fr]"} border-b border-border px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted`}>
        <span>Workflow</span>
        <span>{showProject ? "Project · ID" : "ID"}</span>
        <span>Status</span>
        <span>Duration</span>
        <span>Created</span>
      </div>
      {runs.length === 0 ? (
        <div className="px-4 py-12 text-center text-sm text-secondary">No workflow runs yet.</div>
      ) : (
        runs.map((r) => {
          const sv = statusView(r.status);
          const active = r.status === "running";
          return (
            <Link
              key={r.id}
              href={`/workflows/runs/${encodeURIComponent(r.id)}${r.project ? `?project=${encodeURIComponent(r.project)}` : ""}`}
              className={`grid ${showProject ? "grid-cols-[1.4fr_1.6fr_0.9fr_0.8fr_0.9fr]" : "grid-cols-[1.4fr_1.8fr_0.9fr_0.8fr_0.9fr]"} items-center border-b border-border px-4 py-3 text-sm transition-colors last:border-0 hover:bg-subtle/50`}
            >
              <span className="truncate font-mono">{r.name || r.def_id}</span>
              <span className="truncate font-mono text-xs text-secondary">
                {showProject ? <span className="text-fg">{r.project || "default"} · </span> : null}
                {r.id}
              </span>
              <span className={`flex items-center gap-2 ${sv.text}`}>
                <span className={`h-2 w-2 rounded-full ${sv.dot}`} /> {sv.label}
              </span>
              <span className="flex items-center gap-1.5 tabular-nums text-secondary">
                {fmtDuration(runDuration(r, now))}
                {active && <Loader2 className="h-3 w-3 animate-spin text-muted" />}
              </span>
              <span className="text-muted">{timeAgo(r.started_ms)} ago</span>
            </Link>
          );
        })
      )}
    </div>
  );
}

/* ---- project-scoped Workflows sub-tab ---- */
function useNow(ms = 1000) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(id);
  }, [ms]);
  return now;
}

export function ProjectWorkflows({ project, view = "runs" }: { project: string; view?: WdkView }) {
  const { data: rawRuns, refresh } = usePoll<WorkflowRun[]>(
    `/v1/workflows/runs?project=${encodeURIComponent(project)}`,
    2000
  );
  // Workflow DEFINITIONS for this project (drives the Workflows + Hooks sub-views).
  const { data: rawDefs, refresh: refreshDefs } = usePoll<WorkflowDef[]>("/v1/workflows", 5000);
  const runs = useMemo(() => (rawRuns ?? []).map(normalizeRun), [rawRuns]);
  const defs = useMemo(
    () => (rawDefs ?? []).filter((d) => (d.project || "") === project),
    [rawDefs, project],
  );
  const now = useNow();
  const [q, setQ] = useState("");
  const [live, setLive] = useState(true);
  const [creating, setCreating] = useState(false);

  const filtered = useMemo(() => {
    const needle = q.toLowerCase();
    return runs.filter(
      (r) => r.name.toLowerCase().includes(needle) || r.id.toLowerCase().includes(needle)
    );
  }, [runs, q]);

  // Workflows + Hooks sub-views are project-scoped mirrors of the platform page.
  if (view === "workflows") {
    return <ProjectDefsView project={project} defs={defs} onChanged={refreshDefs} />;
  }
  if (view === "hooks") {
    return <ProjectHooksView defs={defs} />;
  }

  return (
    <div>
      {/* Controls */}
      <div className="mb-4 flex flex-wrap items-center gap-2">
        <div className="relative flex-1 min-w-[200px]">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <Input placeholder="Search workflows, runs, and hooks…" value={q} onChange={(e) => setQ(e.target.value)} className="pl-9" />
        </div>
        <button
          onClick={() => setLive((v) => !v)}
          className={`flex items-center gap-1.5 rounded-md border px-3 py-1.5 text-sm ${live ? "border-[#0070f3] text-[#0070f3]" : "border-border-strong text-secondary hover:bg-subtle"}`}
        >
          <Radio className="h-3.5 w-3.5" /> Live
        </button>
        <Button variant="outline" onClick={() => setCreating((v) => !v)}>
          {creating ? <X className="h-4 w-4" /> : <Plus className="h-4 w-4" />} {creating ? "Close" : "New Workflow"}
        </Button>
      </div>

      {creating && <ProjectDefiner project={project} onDone={() => { setCreating(false); refresh(); refreshDefs(); }} />}

      <div className="mb-4">
        <StatusChips runs={runs ?? []} />
      </div>

      <RunsTable runs={filtered} now={now} />
    </div>
  );
}

/** Project-scoped "Workflows" definitions list (mirror of the platform view). */
function ProjectDefsView({ project, defs, onChanged }: { project: string; defs: WorkflowDef[]; onChanged: () => void }) {
  const [creating, setCreating] = useState(false);
  async function run(id: string) {
    await apiSend("POST", `/v1/workflows/${encodeURIComponent(id)}/run`).catch(() => {});
  }
  return (
    <div className="space-y-4">
      <div className="flex justify-end">
        <Button variant="outline" onClick={() => setCreating((v) => !v)}>
          {creating ? <X className="h-4 w-4" /> : <Plus className="h-4 w-4" />} {creating ? "Close" : "New Workflow"}
        </Button>
      </div>
      {creating && <ProjectDefiner project={project} onDone={() => { setCreating(false); onChanged(); }} />}
      <div className="overflow-hidden rounded-xl border border-border bg-card">
        <div className="grid grid-cols-[1.6fr_1.6fr_auto] border-b border-border px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
          <span>Workflow</span><span>Steps</span><span></span>
        </div>
        {defs.length === 0 ? (
          <div className="px-4 py-12 text-center text-sm text-secondary">No workflows defined for this project yet — use “New Workflow”, or deploy an app using the Vercel WDK.</div>
        ) : (
          defs.map((d) => (
            <div key={d.id} className="grid grid-cols-[1.6fr_1.6fr_auto] items-center gap-3 border-b border-border px-4 py-3 text-sm last:border-0">
              <span className="truncate font-mono font-medium">{d.name}</span>
              <span className="truncate font-mono text-xs text-muted">{d.steps.map((s) => s.name).join(" → ") || "—"}</span>
              <Button variant="outline" className="px-2.5 py-1 text-xs" onClick={() => run(d.id)}>Run</Button>
            </div>
          ))
        )}
      </div>
    </div>
  );
}

/** Project-scoped "Hooks" view — the trigger URL for each workflow. */
function ProjectHooksView({ defs }: { defs: WorkflowDef[] }) {
  return (
    <div className="space-y-3">
      <p className="text-sm text-secondary">Trigger workflows from anything — POST to a hook URL to start a run.</p>
      {defs.length === 0 ? (
        <div className="rounded-xl border border-border bg-card px-4 py-12 text-center text-sm text-secondary">No hooks — define a workflow for this project first.</div>
      ) : (
        defs.map((d) => (
          <div key={d.id} className="rounded-xl border border-border bg-card p-4">
            <div className="mb-1 font-mono text-sm font-medium">{d.name}</div>
            <code className="block overflow-x-auto rounded-md border border-border bg-subtle/50 px-3 py-2 font-mono text-xs text-secondary">
              POST /cloud/v1/workflows/{d.id}/run
            </code>
          </div>
        ))
      )}
    </div>
  );
}

function ProjectDefiner({ project, onDone }: { project: string; onDone: () => void }) {
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
      onDone();
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="mb-4 rounded-xl border border-border bg-card p-4">
      <div className="mb-3 text-sm font-medium">New workflow in {project}</div>
      <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
        <Input placeholder="name" value={name} onChange={(e) => setName(e.target.value)} />
        <Input placeholder="deployment alias" value={deployment} onChange={(e) => setDeployment(e.target.value)} />
        <Input placeholder="step paths, comma-separated" value={steps} onChange={(e) => setSteps(e.target.value)} />
      </div>
      <div className="mt-3 flex gap-2">
        <Button onClick={defineAndRun} disabled={busy || !deployment}>{busy ? "Starting…" : "Define & Run"}</Button>
        <Button variant="ghost" onClick={onDone}>Cancel</Button>
      </div>
    </div>
  );
}
