"use client";

import { useEffect, useState } from "react";
import dynamic from "next/dynamic";
import Link from "next/link";
import { ChevronRight, ScrollText, MoreHorizontal, ChevronDown, Loader2 } from "lucide-react";
import { usePoll, type WorkflowRun, type StepRun } from "@/lib/api";
import { Triangle } from "@/components/ui";
import { statusView, fmtDuration, runDuration } from "@/components/workflows";

// Lazy-load the React Flow manifest canvas (heavy) — only when the Graph tab opens.
const WorkflowGraph = dynamic(() => import("@/components/workflow-graph").then((m) => m.WorkflowGraph), {
  ssr: false,
  loading: () => (
    <div className="flex h-[460px] items-center justify-center rounded-xl border border-border bg-bg">
      <Loader2 className="h-5 w-5 animate-spin text-muted" />
    </div>
  ),
});

function useNow(ms = 1000) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(id);
  }, [ms]);
  return now;
}

type Tab = "trace" | "graph" | "events" | "streams";

export default function RunDetailPage({ params }: { params: { id: string } }) {
  const id = decodeURIComponent(params.id);
  const { data: run } = usePoll<WorkflowRun>(`/v1/workflows/runs/${encodeURIComponent(id)}`, 1500);
  const now = useNow();
  const [tab, setTab] = useState<Tab>("trace");
  const [selected, setSelected] = useState<number | null>(null);

  if (!run) {
    return <div className="py-20 text-center text-sm text-secondary">Loading run…</div>;
  }

  const sv = statusView(run.status);
  const total = runDuration(run, now);
  const sel = selected != null ? run.steps[selected] : null;

  return (
    <div className="pb-20">
      {/* Breadcrumb */}
      <div className="mb-5 flex items-center gap-1.5 text-sm text-secondary">
        <Link href={`/projects/${encodeURIComponent(run.project)}?tab=workflows`} className="flex items-center gap-1.5 hover:text-fg">
          <Triangle className="h-4 w-4" /> {run.project || "default"}
        </Link>
        <ChevronRight className="h-3.5 w-3.5 text-muted" />
        <Link href="/workflows" className="hover:text-fg">Workflows</Link>
        <ChevronRight className="h-3.5 w-3.5 text-muted" />
        <span className="font-mono text-fg">{run.id}</span>
      </div>

      {/* Title */}
      <div className="mb-5 flex items-center justify-between">
        <div className="flex items-center gap-3">
          <span className="text-xl font-semibold">{run.name || run.def_id}</span>
          <span className="font-mono text-sm text-muted">{run.id}</span>
          <span className={`flex items-center gap-1.5 text-sm ${sv.text}`}>
            <span className={`h-2 w-2 rounded-full ${sv.dot}`} /> {sv.label}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <Link href={`/projects/${encodeURIComponent(run.project)}/logs`} className="flex items-center gap-1.5 rounded-md border border-border-strong px-3 py-1.5 text-sm hover:bg-subtle">
            <ScrollText className="h-3.5 w-3.5" /> View Logs
          </Link>
          <button className="flex h-8 w-8 items-center justify-center rounded-md border border-border-strong text-muted hover:bg-subtle"><MoreHorizontal className="h-4 w-4" /></button>
        </div>
      </div>

      {/* Meta */}
      <div className="mb-6 grid grid-cols-2 gap-4 rounded-xl border border-border bg-card px-5 py-4 sm:grid-cols-5">
        <Meta label="Created" value={`${fmtDuration(now - run.started_ms)} ago`} />
        <Meta label="Completed" value={run.finished_ms ? `${fmtDuration(now - run.finished_ms)} ago` : "—"} />
        <Meta label="Duration" value={fmtDuration(total)} />
        <Meta label="Steps" value={String(run.steps.length)} />
        <Meta label="Storage" value={`${Math.max(1, run.steps.length)} KB`} />
      </div>

      {/* Tabs */}
      <div className="mb-4 flex items-center gap-1 rounded-lg border border-border bg-card p-1 w-fit">
        {(["trace", "graph", "events", "streams"] as const).map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`rounded-md px-3 py-1 text-sm capitalize transition-colors ${tab === t ? "bg-subtle font-medium text-fg" : "text-secondary hover:text-fg"}`}
          >
            {t}
          </button>
        ))}
      </div>

      {tab === "trace" && (
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-[1fr_360px]">
          <Trace run={run} now={now} selected={selected} onSelect={setSelected} />
          {sel ? <StepPanel step={sel} run={run} now={now} onClose={() => setSelected(null)} /> : (
            <div className="hidden rounded-xl border border-dashed border-border p-6 text-center text-sm text-muted lg:block">
              Select a span to inspect its output and events.
            </div>
          )}
        </div>
      )}

      {tab === "graph" && <WorkflowGraph run={run} now={now} />}
      {tab === "events" && <Events run={run} />}
      {tab === "streams" && (
        <div className="rounded-xl border border-border bg-card px-4 py-12 text-center text-sm text-secondary">No streams for this run.</div>
      )}
    </div>
  );
}

function Meta({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-xs text-muted">{label}</div>
      <div className="mt-0.5 text-sm tabular-nums">{value}</div>
    </div>
  );
}

/* ---- Trace (gantt) ---- */
function Trace({ run, now, selected, onSelect }: { run: WorkflowRun; now: number; selected: number | null; onSelect: (i: number | null) => void }) {
  const start = run.started_ms;
  const end = run.finished_ms ?? now;
  const total = Math.max(1, end - start);

  // ~6 evenly spaced time ticks.
  const ticks = Array.from({ length: 7 }, (_, i) => Math.round((total / 6) * i));

  const barColor = (s: StepRun) =>
    s.status === "failed" ? "bg-red-500" : s.status === "running" ? "bg-amber-500" : "bg-emerald-500/80";

  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      {/* Time axis */}
      <div className="relative h-7 border-b border-border text-[10px] text-muted">
        {ticks.map((t, i) => (
          <span key={i} className="absolute top-1.5 -translate-x-1/2 tabular-nums" style={{ left: `${(t / total) * 100}%` }}>
            {fmtDuration(t)}
          </span>
        ))}
      </div>

      {/* Root span */}
      <div className="border-b border-border px-3 py-2">
        <div className="relative h-6">
          <div className="absolute inset-y-1 left-0 right-0 flex items-center rounded bg-blue-600/80 px-2 text-xs font-medium text-white">
            <span className="truncate">{run.name || run.def_id}</span>
            <span className="ml-auto tabular-nums">{fmtDuration(total)}</span>
          </div>
        </div>
      </div>

      {/* Step spans */}
      <div className="divide-y divide-border">
        {run.steps.length === 0 && <div className="px-4 py-8 text-center text-sm text-secondary">Waiting for first step…</div>}
        {run.steps.map((s, i) => {
          const sStart = s.started_ms - start;
          const sEnd = (s.finished_ms ?? now) - start;
          const left = Math.min(100, Math.max(0, (sStart / total) * 100));
          const width = Math.max(1.2, ((sEnd - sStart) / total) * 100);
          const active = selected === i;
          return (
            <button
              key={i}
              onClick={() => onSelect(active ? null : i)}
              className={`relative block h-9 w-full px-3 text-left transition-colors hover:bg-subtle/50 ${active ? "bg-subtle/60" : ""}`}
            >
              <div className="relative h-full">
                <div
                  className={`absolute top-1/2 flex h-5 -translate-y-1/2 items-center rounded ${barColor(s)} px-1.5`}
                  style={{ left: `${left}%`, width: `${Math.min(width, 100 - left)}%`, minWidth: 18 }}
                >
                  <span className="truncate text-[11px] font-medium text-white">{s.name}</span>
                </div>
                {/* label to the right of very short bars */}
                {width < 12 && (
                  <span className="absolute top-1/2 -translate-y-1/2 whitespace-nowrap pl-1 text-[11px] text-secondary" style={{ left: `calc(${Math.min(left + width, 100)}% + 4px)` }}>
                    {fmtDuration(sEnd - sStart)}
                  </span>
                )}
              </div>
            </button>
          );
        })}
      </div>
    </div>
  );
}

/* ---- Selected step side panel ---- */
function StepPanel({ step, run, now, onClose }: { step: StepRun; run: WorkflowRun; now: number; onClose: () => void }) {
  const sv = statusView(step.status);
  const dur = (step.finished_ms ?? now) - step.started_ms;
  return (
    <div className="h-fit rounded-xl border border-border bg-card">
      <div className="flex items-center justify-between border-b border-border px-4 py-3">
        <span className="truncate font-mono text-sm">{step.name}</span>
        <button onClick={onClose} className="text-muted hover:text-fg">✕</button>
      </div>
      <div className="space-y-4 p-4 text-sm">
        <div className="flex items-center gap-2">
          <span className={`flex items-center gap-1.5 ${sv.text}`}><span className={`h-2 w-2 rounded-full ${sv.dot}`} /> {sv.label}</span>
          <span className="ml-auto tabular-nums text-secondary">{fmtDuration(dur)}</span>
        </div>
        <Section title="Output">
          <pre className="overflow-x-auto whitespace-pre-wrap break-words rounded-lg border border-border bg-subtle/50 p-3 font-mono text-xs text-secondary">
            {step.output || "(no output yet)"}
          </pre>
        </Section>
        <Section title="Events">
          <div className="space-y-1.5 text-xs">
            <EventRow label="step_created" ts={step.started_ms} />
            <EventRow label="step_started" ts={step.started_ms} />
            {step.finished_ms && <EventRow label={step.status === "failed" ? "step_failed" : "step_completed"} ts={step.finished_ms} />}
          </div>
        </Section>
        <div className="text-xs text-muted">Run {run.id}</div>
      </div>
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="mb-1.5 flex items-center gap-1 text-xs font-medium text-secondary"><ChevronDown className="h-3.5 w-3.5" /> {title}</div>
      {children}
    </div>
  );
}

function EventRow({ label, ts }: { label: string; ts: number }) {
  return (
    <div className="flex items-center justify-between rounded border border-border px-2 py-1">
      <span className="font-mono">{label}</span>
      <span className="tabular-nums text-muted">{new Date(ts).toLocaleTimeString()}</span>
    </div>
  );
}

/* ---- Events tab ---- */
function Events({ run }: { run: WorkflowRun }) {
  const rows: { label: string; ts: number; name: string }[] = [];
  for (const s of run.steps) {
    rows.push({ label: "step_created", ts: s.started_ms, name: s.name });
    rows.push({ label: "step_started", ts: s.started_ms, name: s.name });
    if (s.finished_ms) rows.push({ label: s.status === "failed" ? "step_failed" : "step_completed", ts: s.finished_ms, name: s.name });
  }
  rows.sort((a, b) => a.ts - b.ts);
  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      <div className="grid grid-cols-[1fr_1fr_auto] border-b border-border px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
        <span>Event</span><span>Step</span><span>Time</span>
      </div>
      {rows.length === 0 ? (
        <div className="px-4 py-10 text-center text-sm text-secondary">No events yet.</div>
      ) : rows.map((r, i) => (
        <div key={i} className="grid grid-cols-[1fr_1fr_auto] items-center border-b border-border px-4 py-2.5 text-sm last:border-0">
          <span className="font-mono">{r.label}</span>
          <span className="font-mono text-secondary">{r.name}</span>
          <span className="tabular-nums text-muted">{new Date(r.ts).toLocaleTimeString()}</span>
        </div>
      ))}
    </div>
  );
}
