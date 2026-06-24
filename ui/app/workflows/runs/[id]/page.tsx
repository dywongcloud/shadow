"use client";

import { Suspense, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { ChevronLeft, Loader2 } from "lucide-react";
import { apiGet } from "@/lib/api";

// Timestamps from the WDK world are epoch SECONDS (float). Normalize to ms.
const toMs = (t: unknown): number | null => {
  if (typeof t !== "number") return null;
  return t > 1e12 ? t : t * 1000; // already-ms vs seconds
};
// "workflow//./app/workflows/session//sessionWorkflow" → "sessionWorkflow"
function cleanName(n?: string): string {
  if (!n) return "workflow";
  const parts = n.split("/").filter(Boolean);
  return parts[parts.length - 1] || n;
}
function fmtDur(ms: number | null): string {
  if (ms == null || ms < 0) return "—";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const m = Math.floor(ms / 60_000);
  return `${m}m ${Math.round((ms % 60_000) / 1000)}s`;
}
function fmtTime(ms: number | null): string {
  if (ms == null) return "—";
  return new Date(ms).toLocaleString();
}

type Status = string;
const STATUS_COLOR: Record<string, string> = {
  completed: "#16a34a",
  succeeded: "#16a34a",
  running: "#0070f3",
  pending: "#a1a1aa",
  queued: "#a1a1aa",
  failed: "#dc2626",
  error: "#dc2626",
  cancelled: "#d97706",
};
const colorFor = (s: Status) => STATUS_COLOR[(s || "").toLowerCase()] || "#71717a";

function StatusBadge({ status }: { status: string }) {
  const c = colorFor(status);
  const running = (status || "").toLowerCase() === "running";
  return (
    <span className="inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium" style={{ background: `${c}1a`, color: c }}>
      <span className={`h-1.5 w-1.5 rounded-full ${running ? "animate-pulse" : ""}`} style={{ background: c }} />
      {status || "unknown"}
    </span>
  );
}

interface Step {
  stepId?: string;
  stepName?: string;
  status?: string;
  attempt?: number;
  startedAt?: number;
  completedAt?: number;
  createdAt?: number;
  error?: unknown;
}
interface RunDetail {
  run?: Record<string, any>;
  steps?: Step[];
  events?: Record<string, any>[];
}

export default function RunDetailPage({ params }: { params: { id: string } }) {
  return (
    <Suspense fallback={<div className="mx-auto max-w-5xl px-4 py-6 text-sm text-secondary">Loading run…</div>}>
      <RunDetail id={params.id} />
    </Suspense>
  );
}

function RunDetail({ id }: { id: string }) {
  const runId = decodeURIComponent(id);
  const project = useSearchParams().get("project") || "";
  const [data, setData] = useState<RunDetail | null>(null);
  const [err, setErr] = useState("");
  const [now, setNow] = useState(() => Date.now());
  const [tab, setTab] = useState<"gantt" | "events" | "io">("gantt");

  const run = data?.run ?? {};
  const status = (run.status as string) || "";
  const isLive = ["running", "pending", "queued"].includes(status.toLowerCase());

  // Poll the run detail. Poll fast while live, slow once terminal.
  useEffect(() => {
    let stop = false;
    const path = `/v1/workflows/runs/${encodeURIComponent(runId)}${project ? `?project=${encodeURIComponent(project)}` : ""}`;
    const tick = async () => {
      try {
        const d = await apiGet<RunDetail>(path);
        if (!stop) setData(d);
      } catch (e) {
        if (!stop) setErr(String(e));
      }
    };
    tick();
    const iv = setInterval(tick, isLive ? 1500 : 6000);
    return () => { stop = true; clearInterval(iv); };
  }, [runId, project, isLive]);

  // Clock for live bar growth.
  useEffect(() => {
    if (!isLive) return;
    const iv = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(iv);
  }, [isLive]);

  const steps = data?.steps ?? [];

  // Gantt timeline window: run start → run end (or now if live).
  const { t0, t1 } = useMemo(() => {
    const starts = [toMs(run.startedAt), toMs(run.createdAt), ...steps.map((s) => toMs(s.startedAt) ?? toMs(s.createdAt))].filter((x): x is number => x != null);
    const ends = [toMs(run.completedAt), ...steps.map((s) => toMs(s.completedAt))].filter((x): x is number => x != null);
    const start = starts.length ? Math.min(...starts) : now - 1000;
    const end = isLive ? Math.max(now, ...(ends.length ? ends : [start])) : ends.length ? Math.max(...ends) : start + 1000;
    return { t0: start, t1: Math.max(end, start + 1) };
  }, [data, now, isLive]);
  const span = Math.max(t1 - t0, 1);

  const runStart = toMs(run.startedAt) ?? toMs(run.createdAt);
  const runEnd = toMs(run.completedAt) ?? (isLive ? now : null);
  const runDur = runStart != null ? (runEnd ?? now) - runStart : null;

  return (
    <div className="mx-auto max-w-5xl px-4 py-6">
      <Link href="/workflows" className="mb-4 inline-flex items-center gap-1 text-sm text-secondary hover:text-fg">
        <ChevronLeft className="h-4 w-4" /> Workflows
      </Link>

      {/* Header */}
      <div className="mb-5 flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <h1 className="truncate text-xl font-semibold">{cleanName(run.workflowName)}</h1>
            <StatusBadge status={status} />
            {isLive && <Loader2 className="h-4 w-4 animate-spin text-[#0070f3]" />}
          </div>
          <div className="mt-1 font-mono text-xs text-muted">{runId}</div>
        </div>
        <div className="flex gap-5 text-sm">
          <div><div className="text-xs text-muted">Started</div><div>{fmtTime(runStart)}</div></div>
          <div><div className="text-xs text-muted">Duration</div><div>{fmtDur(runDur)}</div></div>
          <div><div className="text-xs text-muted">Steps</div><div>{steps.length}</div></div>
        </div>
      </div>

      {err && !data && <p className="text-sm text-red-600">{err}</p>}

      {/* Tabs */}
      <div className="mb-4 flex gap-1 border-b border-border">
        {(["gantt", "events", "io"] as const).map((t) => (
          <button key={t} onClick={() => setTab(t)}
            className={`px-3 py-2 text-sm capitalize ${tab === t ? "border-b-2 border-fg font-medium" : "text-secondary hover:text-fg"}`}>
            {t === "gantt" ? "Timeline" : t === "io" ? "Input / Output" : "Events"}
          </button>
        ))}
      </div>

      {/* Gantt timeline */}
      {tab === "gantt" && (
        <div className="space-y-1.5">
          {!steps.length && <p className="py-8 text-center text-sm text-secondary">No steps recorded yet.</p>}
          {steps.map((s, i) => {
            const st = toMs(s.startedAt) ?? toMs(s.createdAt) ?? t0;
            const en = toMs(s.completedAt) ?? (["running", "pending"].includes((s.status || "").toLowerCase()) ? now : st);
            const left = ((st - t0) / span) * 100;
            const width = Math.max(((en - st) / span) * 100, 0.6);
            const c = colorFor(s.status || "");
            const live = (s.status || "").toLowerCase() === "running";
            return (
              <div key={s.stepId || i} className="grid grid-cols-[220px_1fr] items-center gap-3">
                <div className="flex items-center gap-2 truncate">
                  <span className="h-2 w-2 shrink-0 rounded-full" style={{ background: c }} />
                  <span className="truncate text-sm" title={s.stepName}>{cleanName(s.stepName) || s.stepId}</span>
                  {(s.attempt ?? 1) > 1 && <span className="rounded bg-amber-500/15 px-1 text-[10px] text-amber-600">retry {s.attempt}</span>}
                </div>
                <div className="relative h-6 rounded bg-subtle">
                  <div
                    className={`absolute top-0.5 bottom-0.5 rounded ${live ? "animate-pulse" : ""}`}
                    style={{ left: `${left}%`, width: `${width}%`, background: c, minWidth: 3 }}
                    title={`${s.status} · ${fmtDur(en - st)}`}
                  />
                  <span className="absolute right-1.5 top-1/2 -translate-y-1/2 text-[10px] text-muted">{fmtDur(en - st)}</span>
                </div>
              </div>
            );
          })}
          {/* time axis */}
          {steps.length > 0 && (
            <div className="grid grid-cols-[220px_1fr] gap-3 pt-1">
              <div />
              <div className="flex justify-between text-[10px] text-muted">
                <span>0ms</span><span>{fmtDur(span / 2)}</span><span>{fmtDur(span)}</span>
              </div>
            </div>
          )}
        </div>
      )}

      {/* Events */}
      {tab === "events" && (
        <div className="space-y-2">
          {!(data?.events?.length) && <p className="py-8 text-center text-sm text-secondary">No events.</p>}
          {(data?.events ?? []).map((e, i) => (
            <div key={e.eventId || i} className="flex items-start gap-3 rounded-lg border border-border p-3">
              <span className="mt-0.5 font-mono text-xs text-muted">{fmtTime(toMs(e.createdAt))}</span>
              <div className="min-w-0">
                <div className="text-sm font-medium">{e.eventType || "event"}</div>
              </div>
            </div>
          ))}
        </div>
      )}

      {/* Input / Output / Error */}
      {tab === "io" && (
        <div className="space-y-4">
          {run.error != null && (
            <div className="rounded-lg border border-red-500/40 bg-red-500/5 p-3">
              <div className="mb-1 text-xs font-medium text-red-600">Error</div>
              <pre className="overflow-x-auto whitespace-pre-wrap break-all text-xs text-red-700 dark:text-red-400">{JSON.stringify(run.error, null, 2)}</pre>
            </div>
          )}
          <div>
            <div className="mb-1 text-xs font-medium text-muted">Steps</div>
            <div className="space-y-1">
              {steps.map((s, i) => (
                <div key={s.stepId || i} className="flex items-center justify-between rounded border border-border px-3 py-1.5 text-sm">
                  <span className="truncate">{cleanName(s.stepName) || s.stepId}</span>
                  <StatusBadge status={s.status || ""} />
                </div>
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
