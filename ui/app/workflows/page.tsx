"use client";

import { useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { Search, ChevronDown, Calendar, Plus, X } from "lucide-react";
import { Button, Input, Triangle, PageHeader } from "@/components/ui";
import { apiSend, usePoll, type WorkflowRun, type WorkflowDef } from "@/lib/api";
import { StatusChips, RunsTable, normalizeRun } from "@/components/workflows";
import { WorkflowDefGraph } from "@/components/workflow-graph";

const RANGES: Record<string, number> = {
  "Last 1 hour": 3600_000,
  "Last 12 hours": 12 * 3600_000,
  "Last 24 hours": 24 * 3600_000,
  "Last 7 days": 7 * 24 * 3600_000,
};

function useNow(ms = 1000) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(id);
  }, [ms]);
  return now;
}

export default function WorkflowsPage() {
  const { data: rawRuns } = usePoll<WorkflowRun[]>("/v1/workflows/runs", 2000);
  const runs = useMemo(() => (rawRuns ?? []).map(normalizeRun), [rawRuns]);
  const { data: defs } = usePoll<WorkflowDef[]>("/v1/workflows", 5000);
  const now = useNow();
  const [q, setQ] = useState("");
  const [range, setRange] = useState("Last 12 hours");
  const [env, setEnv] = useState("All Environments");
  const [creating, setCreating] = useState(false);
  const [view, setView] = useState<WdkView>("runs");

  // Time window, then environment scope. (Workflow runs execute on the deployed
  // app, so they default to "production"; the env filter narrows the same dataset.)
  const windowed = useMemo(() => {
    const cutoff = now - (RANGES[range] ?? RANGES["Last 12 hours"]);
    return (runs ?? []).filter((r) => r.started_ms >= cutoff);
  }, [runs, range, now]);

  const envScoped = useMemo(() => {
    if (env === "All Environments") return windowed;
    const want = env.toLowerCase();
    return windowed.filter((r) => (r.environment ?? "production") === want);
  }, [windowed, env]);

  // Per-project activity rollup derived from the REAL runs (created / completed /
  // failed / active), so the table reflects live data + the env/time filters —
  // this is what was previously empty (the summary endpoint ignored WDK world runs).
  const summary = useMemo(() => {
    const agg = new Map<string, { project: string; created: number; completed: number; failed: number; active: number }>();
    for (const r of envScoped) {
      const proj = r.project || "default";
      const e = agg.get(proj) ?? { project: proj, created: 0, completed: 0, failed: 0, active: 0 };
      e.created++;
      if (r.status === "succeeded") e.completed++;
      else if (r.status === "failed") e.failed++;
      else if (r.status === "running" || r.status === "pending") e.active++;
      agg.set(proj, e);
    }
    return [...agg.values()].sort((a, b) => b.created - a.created);
  }, [envScoped]);

  const filtered = useMemo(() => {
    const needle = q.toLowerCase();
    return envScoped.filter(
      (r) =>
        r.name.toLowerCase().includes(needle) ||
        r.id.toLowerCase().includes(needle) ||
        r.project.toLowerCase().includes(needle)
    );
  }, [envScoped, q]);

  return (
    <div className="pb-20">
      {/* Header — consistent with every other page (PageHeader). */}
      <PageHeader
        title="Workflows"
        desc="Compose multi-step pipelines across your deployments and run them on demand or on a schedule."
        action={
          <div className="flex items-center gap-2">
            <Pill icon={<Calendar className="h-3.5 w-3.5" />} label={range} options={Object.keys(RANGES)} onSelect={setRange} />
            <Button variant="outline" onClick={() => setCreating((v) => !v)}>
              {creating ? <X className="h-4 w-4" /> : <Plus className="h-4 w-4" />} {creating ? "Close" : "New Workflow"}
            </Button>
          </div>
        }
      />

      {creating && <WorkflowDefiner onDone={() => setCreating(false)} />}

      {/* WDK-web style sub-tabs (Workflows enabled + visible). */}
      <div className="mb-5 flex items-center gap-1 border-b border-border">
        {(["runs", "workflows", "hooks"] as const).map((v) => (
          <button
            key={v}
            onClick={() => setView(v)}
            className={`relative px-3 py-2 text-sm capitalize ${view === v ? "text-fg" : "text-secondary hover:text-fg"}`}
          >
            {v}
            {view === v && <span className="absolute inset-x-2 -bottom-px h-0.5 rounded-full bg-fg" />}
          </button>
        ))}
      </div>

      {view === "runs" && (
        <>
          <div className="mb-4"><StatusChips runs={envScoped} /></div>
          <div className="mb-3 flex items-center gap-2">
            <Pill label={env} options={["All Environments", "Production", "Preview"]} onSelect={setEnv} />
            <div className="relative flex-1">
              <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
              <Input placeholder="Search workflows, runs, and projects…" value={q} onChange={(e) => setQ(e.target.value)} className="pl-9" />
            </div>
          </div>
          <div className="mb-6 overflow-hidden rounded-xl border border-border bg-card">
            <div className="grid grid-cols-[2fr_0.8fr_0.8fr_0.8fr_auto] border-b border-border px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
              <span>Project</span><span>Created</span><span>Completed</span><span>Failed</span><span></span>
            </div>
            {(summary ?? []).length === 0 ? (
              <div className="px-4 py-10 text-center text-sm text-secondary">No workflow activity yet.</div>
            ) : (
              (summary ?? []).map((s) => (
                <Link
                  key={s.project}
                  href={`/projects/${encodeURIComponent(s.project)}?tab=workflows`}
                  className="grid grid-cols-[2fr_0.8fr_0.8fr_0.8fr_auto] items-center border-b border-border px-4 py-3 text-sm transition-colors last:border-0 hover:bg-subtle/50"
                >
                  <span className="flex items-center gap-2 font-medium"><Triangle className="h-5 w-5" /> {s.project}</span>
                  <span className="tabular-nums">{s.created}</span>
                  <span className="tabular-nums text-emerald-600 dark:text-emerald-400">{s.completed}</span>
                  <span className="tabular-nums text-red-600 dark:text-red-400">{s.failed}</span>
                  <ChevronDown className="h-4 w-4 -rotate-90 text-muted" />
                </Link>
              ))
            )}
          </div>
          <div className="mb-2 text-sm font-medium">Recent runs</div>
          <RunsTable runs={filtered} now={now} showProject />
        </>
      )}

      {view === "workflows" && <WorkflowsView defs={defs ?? []} />}
      {view === "hooks" && <HooksView defs={defs ?? []} />}
    </div>
  );
}

type WdkView = "runs" | "workflows" | "hooks";

/** The "Workflows" definitions view (enabled + visible, unlike WDK's default).
 *  Workflows ingested from a deployed app's Vercel WDK manifest carry a `graph`,
 *  which we render on the canvas (Vercel web-dashboard style) via "Diagram". */
function WorkflowsView({ defs }: { defs: WorkflowDef[] }) {
  const [openId, setOpenId] = useState<string | null>(null);
  async function run(id: string) {
    await apiSend("POST", `/v1/workflows/${encodeURIComponent(id)}/run`).catch(() => {});
  }
  const open = defs.find((d) => d.id === openId) || null;
  return (
    <div className="space-y-4">
      <div className="overflow-hidden rounded-xl border border-border bg-card">
        <div className="grid grid-cols-[1.4fr_1fr_1.4fr_auto] border-b border-border px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
          <span>Workflow</span><span>Project</span><span>Steps</span><span></span>
        </div>
        {defs.length === 0 ? (
          <div className="px-4 py-12 text-center text-sm text-secondary">No workflows defined yet — deploy an app using the Vercel WDK, or use “New Workflow”.</div>
        ) : (
          defs.map((d) => {
            const hasGraph = !!(d.graph && d.graph.nodes && d.graph.nodes.length);
            return (
              <div key={d.id} className="grid grid-cols-[1.4fr_1fr_1.4fr_auto] items-center gap-3 border-b border-border px-4 py-3 text-sm last:border-0">
                <span className="truncate font-mono font-medium">{d.name}</span>
                <span className="truncate text-secondary">{d.project || "default"}</span>
                <span className="truncate font-mono text-xs text-muted">{d.steps.map((s) => s.name).join(" → ") || (hasGraph ? `${d.graph!.nodes.length} node(s)` : "—")}</span>
                <div className="flex gap-2">
                  {hasGraph && (
                    <Button variant="outline" className="px-2.5 py-1 text-xs" onClick={() => setOpenId(openId === d.id ? null : d.id)}>
                      {openId === d.id ? "Hide" : "Diagram"}
                    </Button>
                  )}
                  <Button variant="outline" className="px-2.5 py-1 text-xs" onClick={() => run(d.id)}>Run</Button>
                </div>
              </div>
            );
          })
        )}
      </div>
      {open && open.graph && (
        <div>
          <div className="mb-2 text-sm font-medium">{open.name} — workflow graph</div>
          <WorkflowDefGraph def={open} />
        </div>
      )}
    </div>
  );
}

/** The "Hooks" view — trigger endpoints for each workflow. */
function HooksView({ defs }: { defs: WorkflowDef[] }) {
  return (
    <div className="space-y-3">
      <p className="text-sm text-secondary">Trigger workflows from anything — POST to a hook URL to start a run.</p>
      {defs.length === 0 ? (
        <div className="rounded-xl border border-border bg-card px-4 py-12 text-center text-sm text-secondary">No hooks — define a workflow first.</div>
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

/** Small dropdown pill used for env/time-range filters. */
function Pill({ icon, label, options, onSelect }: { icon?: React.ReactNode; label: string; options: string[]; onSelect: (v: string) => void }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="relative">
      <button
        onClick={() => setOpen((o) => !o)}
        onBlur={() => setTimeout(() => setOpen(false), 150)}
        className="flex items-center gap-2 rounded-md border border-border-strong px-3 py-1.5 text-sm text-fg hover:bg-subtle"
      >
        {icon} {label} <ChevronDown className="h-3.5 w-3.5 text-muted" />
      </button>
      {open && (
        <div className="absolute right-0 z-30 mt-1 w-44 overflow-hidden rounded-lg border border-border bg-card py-1 shadow-pop">
          {options.map((o) => (
            <button key={o} onMouseDown={() => { onSelect(o); setOpen(false); }} className="block w-full px-3 py-1.5 text-left text-sm hover:bg-subtle">
              {o}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/** Define + immediately a workflow against a real project deployment. */
function WorkflowDefiner({ onDone }: { onDone: () => void }) {
  const [name, setName] = useState("pipeline");
  const [project, setProject] = useState("");
  const [deployment, setDeployment] = useState("");
  const [steps, setSteps] = useState("/api/hello,/api/cached");
  const [busy, setBusy] = useState(false);

  async function defineAndRun() {
    if (!project.trim() || !deployment.trim()) return;
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
      <div className="mb-3 text-sm font-medium">New workflow</div>
      <div className="grid grid-cols-1 gap-3 md:grid-cols-4">
        <Input placeholder="name" value={name} onChange={(e) => setName(e.target.value)} />
        <Input placeholder="project" value={project} onChange={(e) => setProject(e.target.value)} />
        <Input placeholder="deployment alias" value={deployment} onChange={(e) => setDeployment(e.target.value)} />
        <Input placeholder="step paths, comma-separated" value={steps} onChange={(e) => setSteps(e.target.value)} />
      </div>
      <div className="mt-3 flex gap-2">
        <Button onClick={defineAndRun} disabled={busy || !project || !deployment}>{busy ? "Starting…" : "Define & Run"}</Button>
        <Button variant="ghost" onClick={onDone}>Cancel</Button>
      </div>
    </div>
  );
}
