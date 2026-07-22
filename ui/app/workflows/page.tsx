"use client";

import { Suspense, useEffect, useMemo, useState } from "react";
import { useSearchParams } from "next/navigation";
import { Plus, X } from "lucide-react";
import { Button, Input, PageHeader } from "@/components/ui";
import { apiSend, usePoll, type WorkflowDef, type WorkflowRun } from "@/lib/api";
import { RunsList, WorkflowsList, HooksView, normalizeRun, type WdkView } from "@/components/workflows";
import { normStatus } from "@/components/workflows/shared";

// ---------------------------------------------------------------------------
// The global workflow observability console — a faithful native port of the
// Vercel @workflow/web Home view (Runs / Hooks / Workflows), aggregated across
// every project, inside hive's chrome. Sub-views are driven by the TOP-NAV
// breadcrumb sub-tabs (`?tab=`), not an in-page tab bar.
// ---------------------------------------------------------------------------

export default function WorkflowsPage() {
  // useSearchParams must sit under a Suspense boundary in the app router.
  return (
    <Suspense fallback={<div className="h-40" />}>
      <WorkflowsInner />
    </Suspense>
  );
}

function WorkflowsInner() {
  const searchParams = useSearchParams();
  const tab = searchParams.get("tab");
  const view: WdkView = tab === "workflows" || tab === "hooks" ? tab : "runs";

  // ADAPTIVE polling: /v1/workflows/runs is the most expensive read on the
  // dashboard (fleet fan-out + per-project Upstash world reads). Poll fast (3s)
  // only while a run is in flight; 12s at rest. Gate on the Runs view so the
  // other tabs stop paying for it entirely.
  const [anyRunning, setAnyRunning] = useState(false);
  const { data: rawRuns, loading: runsLoading } = usePoll<WorkflowRun[]>("/v1/workflows/runs?summary=1", anyRunning ? 3000 : 12000, view === "runs");
  const runs = useMemo(() => (rawRuns ?? []).map(normalizeRun), [rawRuns]);
  useEffect(() => {
    setAnyRunning(runs.some((r) => normStatus(r.status) === "running" || normStatus(r.status) === "pending"));
  }, [runs]);
  const { data: defs } = usePoll<WorkflowDef[]>("/v1/workflows", 15000);
  const [creating, setCreating] = useState(false);

  return (
    <div className="pb-20">
      <PageHeader
        title="Workflows"
        desc="Observe every workflow run, hook, and definition across your deployments."
        action={
          <Button variant="outline" onClick={() => setCreating((v) => !v)}>
            {creating ? <X className="h-4 w-4" /> : <Plus className="h-4 w-4" />} {creating ? "Close" : "New Workflow"}
          </Button>
        }
      />

      {creating && <WorkflowDefiner onDone={() => setCreating(false)} />}

      {view === "runs" && <RunsList runs={rawRuns ? runs : null} loading={runsLoading && !rawRuns} showProject />}
      {view === "workflows" && <WorkflowsList defs={defs ?? []} />}
      {view === "hooks" && <HooksView defs={defs ?? []} />}
    </div>
  );
}

/** Define + immediately run a workflow against a real project deployment. */
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
