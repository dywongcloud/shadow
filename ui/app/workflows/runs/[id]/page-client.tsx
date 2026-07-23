"use client";

import { Suspense, useEffect, useMemo, useState, use } from "react";
import { useSearchParams, useRouter, usePathname } from "next/navigation";
import Link from "next/link";
import dynamic from "next/dynamic";
import {
  Check, ChevronRight, ChevronDown, Code2, Timer, X,
  GitBranch, ArrowUpDown, ExternalLink, Loader2, Waves,
} from "lucide-react";
import { apiGet, usePoll, type WorkflowDef } from "@/lib/api";
import {
  AbsTime, CopyableText, EmptyState, StatusBadge, fmtDuration, decodeBlob, parseName, pretty, statusMeta, toMs, normStatus,
} from "@/components/workflows/shared";
import { RunActions } from "@/components/workflows/run-actions";

// ---------------------------------------------------------------------------
// Run detail — a faithful native port of the Vercel @workflow/web run view:
// breadcrumb + LiveStatus header, upstream stat row (Status / Duration / Run ID
// / Queued / Started / Completed), and the four tabs Trace | Events | Streams |
// Graph. Data comes from the app's own WDK "world" store via
// /v1/workflows/runs/:id → { run, steps, events } (native WDK shapes:
// epoch-second floats; devalue+base64 input/output blobs).
// ---------------------------------------------------------------------------

const WorkflowDefGraph = dynamic(() => import("@/components/workflow-graph").then((m) => m.WorkflowDefGraph), {
  ssr: false,
  loading: () => (
    <div className="flex h-[460px] items-center justify-center rounded-xl border border-border bg-bg">
      <Loader2 className="h-5 w-5 animate-spin text-muted" />
    </div>
  ),
});

interface Detail {
  run?: Record<string, any> | null;
  steps?: Record<string, any>[];
  events?: Record<string, any>[];
  project?: string;
}

interface Span {
  key: string;
  kind: "root" | "step" | "sleep";
  name: string;
  module: string;
  start: number;
  end: number;
  status: string;
  attempt?: number;
  raw: Record<string, any>;
  cid?: string;
}

type RunTab = "trace" | "events" | "streams" | "graph";

export function Page({ paramsPromise }: { paramsPromise: Promise<{ id: string }> }) {
  const params = use(paramsPromise);
  return (
    <Suspense fallback={<div className="p-6 text-sm text-secondary">Loading run…</div>}>
      <RunDetail id={decodeURIComponent(params.id)} />
    </Suspense>
  );
}

function RunDetail({ id }: { id: string }) {
  const sp = useSearchParams();
  const router = useRouter();
  const pathname = usePathname();
  const project = sp.get("project") || "";
  const tabParam = sp.get("tab") as RunTab | null;
  const tab: RunTab = tabParam === "events" || tabParam === "streams" || tabParam === "graph" ? tabParam : "trace";
  const [d, setD] = useState<Detail | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const [sel, setSel] = useState<string | null>(null);

  const path = `/v1/workflows/runs/${encodeURIComponent(id)}${project ? `?project=${encodeURIComponent(project)}` : ""}`;
  const run = d?.run || null;
  const status = String(run?.status || "").toLowerCase();
  const isLive = !!run && !run.completedAt && !["completed", "succeeded", "failed", "cancelled"].includes(status);

  useEffect(() => {
    let stop = false;
    const load = async () => {
      try {
        const r = await apiGet<Detail>(path);
        if (!stop) { setD(r); setErr(null); }
      } catch (e) {
        if (!stop) setErr(String(e));
      }
    };
    load();
    const iv = setInterval(load, isLive ? 1500 : 6000);
    return () => { stop = true; clearInterval(iv); };
  }, [path, isLive]);

  useEffect(() => {
    if (!isLive) return;
    const iv = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(iv);
  }, [isLive]);

  // Workflow definitions — for the Graph tab (match this run's def by id).
  const { data: defs } = usePoll<WorkflowDef[]>("/v1/workflows", 20000, tab === "graph");
  const def = useMemo(() => {
    const wf = run?.workflowName || run?.def_id;
    if (!wf || !defs) return null;
    return defs.find((x) => x.id === wf) ?? defs.find((x) => parseName(x.id).name === parseName(wf).name) ?? null;
  }, [defs, run]);

  const setTab = (t: RunTab) => {
    const q = new URLSearchParams(sp.toString());
    q.set("tab", t);
    router.replace(`${pathname}?${q.toString()}`);
  };

  const steps = useMemo(() => d?.steps || [], [d]);
  const events = useMemo(() => d?.events || [], [d]);

  const spans = useMemo<Span[]>(() => {
    const out: Span[] = [];
    for (const s of steps) {
      const start = toMs(s.startedAt) ?? toMs(s.createdAt);
      if (start == null) continue;
      const end = toMs(s.completedAt) ?? (isLive ? now : start);
      const { module, name } = parseName(s.stepName);
      out.push({
        key: `step:${s.stepId}`, kind: "step", name, module, start, end,
        status: String(s.status || (s.completedAt ? "completed" : "running")),
        attempt: s.attempt, raw: s, cid: s.stepId,
      });
    }
    const waits = events
      .filter((e) => e.eventType === "wait_created" || e.eventType === "wait_completed")
      .sort((a, b) => (toMs(a.createdAt) ?? 0) - (toMs(b.createdAt) ?? 0));
    let pending: Record<string, any> | null = null;
    let wi = 0;
    for (const e of waits) {
      if (e.eventType === "wait_created") pending = e;
      else if (e.eventType === "wait_completed" && pending) {
        const start = toMs(pending.createdAt);
        if (start != null) {
          const end = toMs(e.createdAt) ?? toMs(pending.eventData?.resumeAt) ?? start;
          out.push({ key: `wait:${wi++}`, kind: "sleep", name: "sleep", module: "", start, end, status: "sleep", raw: { ...pending, completed: e }, cid: pending.eventId });
        }
        pending = null;
      }
    }
    if (pending) {
      const start = toMs(pending.createdAt);
      if (start != null) {
        const end = toMs(pending.eventData?.resumeAt) ?? (isLive ? now : start);
        out.push({ key: `wait:${wi++}`, kind: "sleep", name: "sleep", module: "", start, end, status: "sleep", raw: pending, cid: pending.eventId });
      }
    }
    out.sort((a, b) => a.start - b.start);
    return out;
  }, [steps, events, isLive, now]);

  // Active sleeps = wait_created events with no matching wait_completed
  // (correlationId), mirroring the upstream analyzeEvents() gate for the
  // "Cancel Active Sleeps" action.
  const hasPendingSleeps = useMemo(() => {
    const created = new Set<string>();
    const done = new Set<string>();
    for (const e of events) {
      const cid = e.correlationId || e.eventData?.waitId;
      if (!cid) continue;
      if (e.eventType === "wait_created") created.add(cid);
      else if (e.eventType === "wait_completed") done.add(cid);
    }
    for (const c of created) if (!done.has(c)) return true;
    return false;
  }, [events]);

  const queuedMs = toMs(run?.createdAt);
  const startedMs = toMs(run?.startedAt);
  const completedMs = toMs(run?.completedAt);
  const expiredMs = toMs(run?.expiredAt);
  const rootStart = queuedMs ?? startedMs ?? (spans[0]?.start ?? now);
  const rootEnd = completedMs ?? (isLive ? now : Math.max(rootStart, ...spans.map((s) => s.end)));
  const rootSpan: Span | null = run
    ? { key: "root", kind: "root", ...parseName(run.workflowName), start: rootStart, end: rootEnd, status, raw: run, cid: run.runId }
    : null;

  const t0 = rootStart;
  const t1 = Math.max(rootEnd, ...spans.map((s) => s.end), t0 + 1);
  const selected: Span | null = sel === "root" ? rootSpan : spans.find((s) => s.key === sel) || null;
  const wfName = parseName(run?.workflowName).name || id;
  const runError = run?.error ? (typeof run.error === "string" ? run.error : run.error?.message || pretty(run.error)) : null;

  return (
    <div className="pb-10">
      {/* Breadcrumb — Runs / <runId> (upstream), rooted at hive's /workflows. */}
      <div className="mb-4 flex items-center gap-2 text-sm text-secondary">
        <Link href="/workflows" className="hover:text-fg">Runs</Link>
        <span className="text-muted">/</span>
        <span className="font-mono text-fg">{id}</span>
      </div>

      <div className="mb-5 flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="text-xl font-semibold tracking-tight">{wfName}</h1>
          <StatusBadge status={status} live={isLive} />
        </div>
        <div className="flex items-center gap-3">
          {run && project && (
            <RunActions
              runId={id}
              project={project}
              status={status}
              hasPendingSleeps={hasPendingSleeps}
              variant="header"
            />
          )}
          {project && (
            <Link href={`/projects/${encodeURIComponent(project)}?tab=workflows`} className="inline-flex items-center gap-1 text-sm text-link hover:underline">
              {project} <ExternalLink className="h-3 w-3" />
            </Link>
          )}
        </div>
      </div>

      {/* Stat row — upstream vocabulary: Status / Duration / Run ID / Queued /
          Started / Completed (+ Expired when the world retains it), with hive's
          Steps + Spec as secondary extras. */}
      <div className="mb-5 grid grid-cols-2 gap-x-8 gap-y-3 sm:grid-cols-4 lg:grid-cols-8">
        <Stat label="Status"><StatusBadge status={status} live={isLive} className="text-sm" /></Stat>
        <Stat label="Duration"><span className="tabular-nums">{run ? fmtDuration(rootEnd - rootStart) : "—"}</span></Stat>
        <Stat label="Run ID"><CopyableText text={id} className="max-w-full text-xs text-secondary" truncate /></Stat>
        <Stat label="Queued"><AbsTime ms={queuedMs} /></Stat>
        <Stat label="Started"><AbsTime ms={startedMs} /></Stat>
        <Stat label="Completed"><AbsTime ms={completedMs} /></Stat>
        {expiredMs ? (
          <Stat label="Expired"><AbsTime ms={expiredMs} /></Stat>
        ) : (
          <Stat label="Steps"><span className="tabular-nums">{steps.length}</span></Stat>
        )}
        <Stat label="Spec"><span className="tabular-nums">{run?.specVersion != null ? String(run.specVersion) : "—"}</span></Stat>
      </div>

      {runError && (
        <div className="mb-4 overflow-x-auto rounded-lg border border-red-500/40 bg-red-500/5 px-4 py-3 font-mono text-xs text-red-600 dark:text-red-400">
          {runError}
        </div>
      )}

      {/* Tabs — Trace | Events | Streams | Graph (upstream parity). */}
      <div className="mb-4 flex items-center gap-1">
        {(["trace", "events", "streams", "graph"] as const).map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`rounded-md px-3 py-1.5 text-sm capitalize ${tab === t ? "bg-fg text-bg font-medium" : "text-secondary hover:bg-subtle"}`}
          >
            {t}
          </button>
        ))}
      </div>

      {err && !d && <div className="rounded-lg border border-border bg-card px-4 py-10 text-center text-sm text-secondary">Couldn’t load this run: {err}</div>}

      {tab === "trace" && (
        <TraceView rootSpan={rootSpan} spans={spans} t0={t0} t1={t1} selectedKey={sel} onSelect={setSel} selected={selected} events={events} />
      )}
      {tab === "events" && <EventsView events={events} steps={steps} runId={id} />}
      {tab === "streams" && <StreamsView run={run} steps={steps} events={events} />}
      {tab === "graph" && <GraphView def={def} loading={defs === undefined} steps={steps} />}
    </div>
  );
}

/* ------------------------------------------------------------------------- */
/* Trace (gantt) + span side panel                                            */
/* ------------------------------------------------------------------------- */

function TraceView({
  rootSpan, spans, t0, t1, selectedKey, onSelect, selected, events,
}: {
  rootSpan: Span | null; spans: Span[]; t0: number; t1: number;
  selectedKey: string | null; onSelect: (k: string | null) => void; selected: Span | null;
  events: Record<string, any>[];
}) {
  const total = Math.max(1, t1 - t0);
  const ticks = useMemo(() => Array.from({ length: 6 }, (_, i) => t0 + (total * i) / 5), [t0, total]);

  const barMeta = (s: Span) => (s.kind === "sleep" ? { bar: "bg-zinc-300/70 border-zinc-400" } : statusMeta(s.status));

  const Row = ({ s, depth }: { s: Span; depth: number }) => {
    const leftPct = ((s.start - t0) / total) * 100;
    const widthPct = Math.max(0.6, ((s.end - s.start) / total) * 100);
    const m = barMeta(s);
    const active = selectedKey === s.key;
    const Icon = s.kind === "sleep" ? Timer : s.kind === "root" ? GitBranch : Code2;
    return (
      <button
        onClick={() => onSelect(active ? null : s.key)}
        className={`grid w-full grid-cols-[minmax(180px,260px)_1fr] items-center border-b border-border text-left text-sm last:border-0 ${active ? "bg-subtle" : "hover:bg-subtle/50"}`}
      >
        <span className="flex items-center gap-2 truncate px-3 py-2" style={{ paddingLeft: 12 + depth * 14 }}>
          <Icon className={`h-3.5 w-3.5 shrink-0 ${s.kind === "sleep" ? "text-muted" : "text-secondary"}`} />
          <span className={`truncate ${s.kind === "root" ? "font-semibold" : "font-mono text-xs"}`}>{s.name}</span>
          {s.attempt && s.attempt > 1 ? <span className="shrink-0 rounded bg-amber-500/15 px-1 text-[10px] text-amber-600">×{s.attempt}</span> : null}
          <span className="ml-auto shrink-0 pl-2 tabular-nums text-xs text-muted">{fmtDuration(s.end - s.start)}</span>
        </span>
        <span className="relative h-9 border-l border-border">
          <span
            className={`absolute top-1/2 h-3 -translate-y-1/2 rounded-sm border ${m.bar}`}
            style={{ left: `${leftPct}%`, width: `${widthPct}%`, minWidth: 2 }}
            title={`${s.name} · ${fmtDuration(s.end - s.start)}`}
          />
        </span>
      </button>
    );
  };

  return (
    <div className="flex gap-4">
      <div className="min-w-0 flex-1 overflow-hidden rounded-xl border border-border bg-card">
        <div className="grid grid-cols-[minmax(180px,260px)_1fr] border-b border-border bg-subtle/40 text-[11px] text-muted">
          <span className="px-3 py-1.5">Span</span>
          <span className="relative h-7 border-l border-border">
            {ticks.map((t, i) => (
              <span key={i} className="absolute top-1/2 -translate-y-1/2 -translate-x-1/2 tabular-nums" style={{ left: `${(i / (ticks.length - 1)) * 100}%` }}>
                {fmtDuration(t - t0)}
              </span>
            ))}
          </span>
        </div>
        {rootSpan && <Row s={rootSpan} depth={0} />}
        {spans.map((s) => <Row key={s.key} s={s} depth={1} />)}
        {spans.length === 0 && !rootSpan && (
          <EmptyState title="No spans recorded for this run." />
        )}
      </div>

      {selected && <SidePanel span={selected} onClose={() => onSelect(null)} events={events} />}
    </div>
  );
}

function SidePanel({ span, onClose, events }: { span: Span; onClose: () => void; events: Record<string, any>[] }) {
  const raw = span.raw;
  const stepEvents = useMemo(() => {
    if (span.kind === "step") {
      const sn = raw.stepName;
      return events.filter((e) => e.eventType?.startsWith("step_") && e.eventData?.stepName === sn);
    }
    if (span.kind === "root") return events.filter((e) => e.eventType?.startsWith("run_"));
    if (span.kind === "sleep") return [raw, raw.completed].filter(Boolean);
    return [];
  }, [span, events, raw]);

  const input = span.kind === "step" || span.kind === "root" ? decodeBlob(raw.input) : undefined;
  const output = span.kind === "step" || span.kind === "root" ? decodeBlob(raw.output) : undefined;

  return (
    <div className="w-[360px] shrink-0 overflow-hidden rounded-xl border border-border bg-card">
      <div className="flex items-center justify-between border-b border-border px-4 py-2.5">
        <span className="truncate font-mono text-sm font-medium">{span.name}</span>
        <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
      </div>
      <div className="max-h-[70vh] overflow-auto px-4 py-3 text-sm">
        {span.kind !== "sleep" && <Field label="Status"><StatusBadge status={span.status} className="text-xs" /></Field>}
        {span.module && <Field label="Module" mono>{span.module}</Field>}
        {span.kind === "step" && <Field label="Step ID" mono copy>{raw.stepId}</Field>}
        {span.kind === "step" && <Field label="Attempts">{String(raw.attempt ?? 1)}</Field>}
        {raw.specVersion != null && <Field label="Spec Version">{String(raw.specVersion)}</Field>}
        <Field label="Created">{tsStr(raw.createdAt)}</Field>
        {span.kind !== "sleep" && <Field label="Started">{tsStr(raw.startedAt)}</Field>}
        <Field label="Completed">{span.kind === "sleep" ? tsStr(raw.completed?.createdAt) : tsStr(raw.completedAt)}</Field>
        {span.kind === "sleep" && <Field label="Resume At">{tsStr(raw.eventData?.resumeAt)}</Field>}
        {raw.error ? <Field label="Error" mono>{typeof raw.error === "string" ? raw.error : raw.error?.message || pretty(raw.error)}</Field> : null}

        {span.kind !== "sleep" && (
          <>
            <Section label="Input">{input === undefined ? <Empty>(no data)</Empty> : <Json v={input} />}</Section>
            <Section label="Output">{output === undefined ? <Empty>(no data)</Empty> : <Json v={output} />}</Section>
          </>
        )}

        <Section label="Events" defaultOpen>
          <div className="space-y-1">
            {stepEvents.length === 0 ? <Empty>(none)</Empty> : stepEvents.map((e, i) => <EventLine key={e.eventId || i} ev={e} />)}
          </div>
        </Section>
      </div>
    </div>
  );
}

/* ------------------------------------------------------------------------- */
/* Events                                                                     */
/* ------------------------------------------------------------------------- */

function EventsView({ events, steps, runId }: { events: Record<string, any>[]; steps: Record<string, any>[]; runId: string }) {
  const [asc, setAsc] = useState(true);
  const [open, setOpen] = useState<Set<string>>(new Set());

  const stepIdByName = useMemo(() => {
    const m = new Map<string, string>();
    for (const s of steps) if (s.stepName) m.set(s.stepName, s.stepId);
    return m;
  }, [steps]);
  const wfName = useMemo(() => {
    const e = events.find((x) => x.eventData?.workflowName);
    return parseName(e?.eventData?.workflowName).name;
  }, [events]);

  const rows = useMemo(() => {
    const r = [...events].sort((a, b) => (toMs(a.createdAt) ?? 0) - (toMs(b.createdAt) ?? 0));
    return asc ? r : r.reverse();
  }, [events, asc]);

  const cid = (e: Record<string, any>): string => {
    if (e.eventType?.startsWith("run_")) return e.runId || runId;
    if (e.eventType?.startsWith("step_")) return stepIdByName.get(e.eventData?.stepName) || "";
    return e.correlationId || e.eventData?.waitId || "";
  };
  const name = (e: Record<string, any>): string => {
    if (e.eventType?.startsWith("step_")) return parseName(e.eventData?.stepName).name;
    if (e.eventType?.startsWith("run_")) return parseName(e.eventData?.workflowName).name || wfName;
    return "";
  };

  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      <div className="flex items-center justify-between border-b border-border px-3 py-2 text-xs text-muted">
        <span>{events.length} events loaded</span>
        <button onClick={() => setAsc((v) => !v)} className="flex items-center gap-1.5 rounded-md border border-border px-2 py-1 hover:bg-subtle">
          <ArrowUpDown className="h-3 w-3" /> {asc ? "Oldest" : "Newest"}
        </button>
      </div>
      <div className="grid grid-cols-[110px_140px_1fr_1.2fr_1.2fr] border-b border-border px-3 py-2 text-[11px] font-medium uppercase tracking-wide text-muted">
        <span>Time</span><span>Event Type</span><span>Name</span><span>Correlation ID</span><span>Event ID</span>
      </div>
      {rows.length === 0 ? (
        <EmptyState title="No events." />
      ) : rows.map((e, i) => {
        const k = e.eventId || String(i);
        const isOpen = open.has(k);
        return (
          <div key={k} className="border-b border-border last:border-0">
            <button
              onClick={() => setOpen((s) => { const n = new Set(s); n.has(k) ? n.delete(k) : n.add(k); return n; })}
              className="grid w-full grid-cols-[110px_140px_1fr_1.2fr_1.2fr] items-center px-3 py-2 text-left text-xs hover:bg-subtle/50"
            >
              <span className="tabular-nums text-secondary">{timeStr(e.createdAt)}</span>
              <span className="flex items-center gap-1.5"><span className={`h-1.5 w-1.5 rounded-full ${eventDot(e.eventType)}`} /> {e.eventType}</span>
              <span className="truncate font-mono text-secondary">{name(e) || "—"}</span>
              <span className="truncate font-mono text-secondary">{cid(e) || "—"}</span>
              <span className="truncate font-mono text-secondary">{e.eventId}</span>
            </button>
            {isOpen && (
              <div className="border-t border-border bg-bg px-4 py-3">
                <Json v={decodeEventData(e.eventData)} />
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

/* ------------------------------------------------------------------------- */
/* Streams                                                                    */
/* ------------------------------------------------------------------------- */

/** The world-redis adapter keeps no dedicated stream store, so surface any
 *  stream REFERENCES found in the run/step/event payloads (ids like `strm_…`
 *  or offloaded-ref markers), and otherwise state that honestly. */
function StreamsView({ run, steps, events }: { run: Record<string, any> | null; steps: Record<string, any>[]; events: Record<string, any>[] }) {
  const streamIds = useMemo(() => {
    const found = new Set<string>();
    const scan = (v: any, depth: number) => {
      if (depth > 6 || v == null) return;
      if (typeof v === "string") {
        if (/^(strm|stream)_[\w-]+$/.test(v)) found.add(v);
        else if (/^(s3rf:|kvrf:)/.test(v)) found.add(v);
        return;
      }
      if (Array.isArray(v)) { for (const x of v) scan(x, depth + 1); return; }
      if (typeof v === "object") {
        for (const [k, x] of Object.entries(v)) {
          if ((k === "streamId" || k === "stream_id") && typeof x === "string") found.add(x);
          else scan(x, depth + 1);
        }
      }
    };
    if (run) { scan(decodeBlob(run.input), 0); scan(decodeBlob(run.output), 0); }
    for (const s of steps) { scan(decodeBlob(s.input), 0); scan(decodeBlob(s.output), 0); }
    for (const e of events) scan(decodeEventData(e.eventData), 0);
    return [...found];
  }, [run, steps, events]);

  if (streamIds.length === 0) {
    return (
      <div className="overflow-hidden rounded-xl border border-border bg-card">
        <EmptyState
          title="No streams for this run"
          hint="Streams appear when a workflow writes streamed output. This app's world store doesn't retain stream chunks."
          icon={<Waves className="h-6 w-6" />}
        />
      </div>
    );
  }
  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      <div className="grid grid-cols-[1fr] border-b border-border bg-subtle/30 px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
        <span>Stream ID</span>
      </div>
      {streamIds.map((sid) => (
        <div key={sid} className="flex items-center justify-between border-b border-border px-4 py-3 text-sm last:border-0">
          <CopyableText text={sid} className="text-xs text-secondary" truncate />
          <span className="text-xs text-muted">content not retained by this world store</span>
        </div>
      ))}
    </div>
  );
}

/* ------------------------------------------------------------------------- */
/* Graph                                                                      */
/* ------------------------------------------------------------------------- */

function GraphView({ def, loading, steps }: { def: WorkflowDef | null; loading?: boolean; steps: Record<string, any>[] }) {
  // Execution overlay: map step label → status so the def graph reflects this run.
  const overlay = useMemo(() => {
    const m: Record<string, string> = {};
    for (const s of steps) {
      const nm = parseName(s.stepName).name;
      if (nm) m[nm] = normStatus(String(s.status || (s.completedAt ? "completed" : "running")));
    }
    return m;
  }, [steps]);

  if (loading && !def) {
    return (
      <div className="flex h-[460px] items-center justify-center rounded-xl border border-border bg-bg">
        <Loader2 className="h-5 w-5 animate-spin text-muted" />
      </div>
    );
  }
  if (!def || !def.graph || !def.graph.nodes?.length) {
    return (
      <div className="overflow-hidden rounded-xl border border-border bg-card">
        <EmptyState
          title="No graph for this workflow"
          hint="The workflow's manifest carries no declared graph — deploy with the Vercel WDK to publish one."
          icon={<GitBranch className="h-6 w-6" />}
        />
      </div>
    );
  }
  return <WorkflowDefGraph def={def} overlay={overlay} />;
}

/* ------------------------------------------------------------------------- */
/* Small helpers                                                              */
/* ------------------------------------------------------------------------- */

function decodeEventData(data: any): any {
  if (!data || typeof data !== "object") return data;
  const out: any = Array.isArray(data) ? [...data] : { ...data };
  for (const k of ["input", "output", "result"]) {
    if (typeof out[k] === "string") out[k] = decodeBlob(out[k]);
  }
  return out;
}

function eventDot(t?: string): string {
  if (!t) return "bg-zinc-400";
  if (t.endsWith("_completed")) return "bg-emerald-500";
  if (t.endsWith("_started")) return "bg-blue-500";
  if (t.endsWith("_failed") || t.endsWith("_errored")) return "bg-red-500";
  return "bg-zinc-400";
}

function tsStr(t: unknown): string {
  const m = toMs(t);
  return m ? new Date(m).toLocaleString() : "—";
}
function timeStr(t: unknown): string {
  const m = toMs(t);
  if (!m) return "—";
  const dt = new Date(m);
  return `${dt.toLocaleTimeString([], { hour12: false })}.${String(dt.getMilliseconds()).padStart(3, "0")}`;
}
function Stat({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="min-w-0">
      <div className="text-xs text-muted">{label}</div>
      <div className="min-w-0 truncate text-sm">{children}</div>
    </div>
  );
}
function Field({ label, children, mono, copy }: { label: string; children: React.ReactNode; mono?: boolean; copy?: boolean }) {
  return (
    <div className="mb-2 grid grid-cols-[100px_1fr] items-start gap-2">
      <span className="text-xs text-muted">{label}</span>
      {copy ? <CopyableText text={String(children)} className={`break-all ${mono ? "text-xs" : ""}`} mono={mono} />
        : <span className={`break-all ${mono ? "font-mono text-xs" : ""}`}>{children}</span>}
    </div>
  );
}
function Section({ label, children, defaultOpen }: { label: string; children: React.ReactNode; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(!!defaultOpen);
  return (
    <div className="mt-3 border-t border-border pt-2">
      <button onClick={() => setOpen((o) => !o)} className="mb-1 flex items-center gap-1 text-xs font-medium text-secondary">
        {open ? <ChevronDown className="h-3.5 w-3.5" /> : <ChevronRight className="h-3.5 w-3.5" />} {label}
      </button>
      {open && <div>{children}</div>}
    </div>
  );
}
function Empty({ children }: { children: React.ReactNode }) {
  return <span className="text-xs text-muted">{children}</span>;
}
function Json({ v }: { v: any }) {
  return (
    <pre className="max-h-72 overflow-auto whitespace-pre-wrap break-all rounded-md border border-border bg-bg px-2 py-1.5 font-mono text-[11px] leading-relaxed text-secondary">
      {pretty(v)}
    </pre>
  );
}
function EventLine({ ev }: { ev: Record<string, any> }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="rounded-md border border-border">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center justify-between px-2 py-1 text-left text-xs hover:bg-subtle/50">
        <span className="flex items-center gap-1.5"><span className={`h-1.5 w-1.5 rounded-full ${eventDot(ev.eventType)}`} /> {ev.eventType}</span>
        <span className="text-muted">{timeStr(ev.createdAt)}</span>
      </button>
      {open && <div className="border-t border-border p-1.5"><Json v={decodeEventData(ev.eventData)} /></div>}
    </div>
  );
}
