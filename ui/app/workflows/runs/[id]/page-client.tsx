"use client";

import { Suspense, useEffect, useMemo, useState, use } from "react";
import { useSearchParams, useRouter, usePathname } from "next/navigation";
import Link from "next/link";
import {
  Copy, Check, ChevronRight, ChevronDown, Code2, Timer, X,
  GitBranch, ArrowUpDown, ExternalLink,
} from "lucide-react";
import { apiGet } from "@/lib/api";

// ---------------------------------------------------------------------------
// A Vercel-WDK-style run detail: Trace (Gantt) + Events, with a span side panel.
// Data comes from the app's own WDK "world" store via /v1/workflows/runs/:id,
// which returns { run, steps, events } in the native WDK shape (timestamps are
// epoch SECONDS as floats; step/run input+output are devalue+base64 blobs).
// ---------------------------------------------------------------------------

interface Detail {
  run?: Record<string, any> | null;
  steps?: Record<string, any>[];
  events?: Record<string, any>[];
  project?: string;
}

// epoch-seconds (float) | epoch-ms → ms. Returns null for missing/zero.
function ms(t: unknown): number | null {
  if (typeof t !== "number" || t <= 0) return null;
  return t > 1e12 ? t : Math.round(t * 1000);
}

function fmtDur(d: number): string {
  if (!isFinite(d) || d < 0) d = 0;
  if (d < 1000) return `${Math.round(d)}ms`;
  const s = d / 1000;
  if (s < 60) return `${s.toFixed(2).replace(/\.?0+$/, "")}s`;
  const m = Math.floor(s / 60);
  const rem = Math.round(s % 60);
  if (m < 60) return `${m}m ${rem}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

// "step//./app/steps/autopilotSteps//listProactiveTenantsStep" →
//   { module: "./app/steps/autopilotSteps", name: "listProactiveTenantsStep" }
function parseName(raw?: string): { module: string; name: string } {
  if (!raw) return { module: "", name: "" };
  const parts = raw.split("//").map((s) => s.trim()).filter(Boolean);
  if (parts.length >= 3) return { module: parts[1], name: parts[parts.length - 1] };
  if (parts.length === 2) return { module: parts[0], name: parts[1] };
  return { module: "", name: parts[parts.length - 1] || raw };
}

// Minimal devalue `unflatten` — the WDK encodes step/run input+output as
// base64("devl" + devalue.stringify(value)) (with encryption OFF). Decode for the
// side panel / events so we show real values, not opaque blobs. Best-effort.
function unflatten(parsed: any): any {
  if (typeof parsed === "number") {
    if (parsed === -1 || parsed === -2) return undefined;
    if (parsed === -3) return NaN;
    if (parsed === -4) return Infinity;
    if (parsed === -5) return -Infinity;
    if (parsed === -6) return -0;
    throw new Error("bad");
  }
  if (!Array.isArray(parsed) || parsed.length === 0) throw new Error("bad");
  const values = parsed;
  const hydrated: any[] = new Array(values.length);
  const seen = new Set<number>();
  function hydrate(index: number): any {
    if (index === -1 || index === -2) return undefined;
    if (index === -3) return NaN;
    if (index === -4) return Infinity;
    if (index === -5) return -Infinity;
    if (index === -6) return -0;
    if (seen.has(index)) return hydrated[index];
    seen.add(index);
    const value = values[index];
    if (!value || typeof value !== "object") {
      hydrated[index] = value;
    } else if (Array.isArray(value)) {
      if (typeof value[0] === "string") {
        const type = value[0];
        if (type === "Date") hydrated[index] = new Date(value[1]).toISOString();
        else if (type === "BigInt") hydrated[index] = String(value[1]);
        else if (type === "null") {
          const o: any = {};
          hydrated[index] = o;
          for (let i = 1; i < value.length; i += 2) o[value[i]] = hydrate(value[i + 1]);
        } else if (type === "Set") {
          const a: any[] = [];
          hydrated[index] = a;
          for (let i = 1; i < value.length; i++) a.push(hydrate(value[i]));
        } else if (type === "Map") {
          const o: any = {};
          hydrated[index] = o;
          for (let i = 1; i < value.length; i += 2) o[String(hydrate(value[i]))] = hydrate(value[i + 1]);
        } else hydrated[index] = value;
      } else {
        const arr: any[] = new Array(value.length);
        hydrated[index] = arr;
        for (let i = 0; i < value.length; i++) arr[i] = value[i] === -2 ? undefined : hydrate(value[i]);
      }
    } else {
      const obj: any = {};
      hydrated[index] = obj;
      for (const k in value) obj[k] = hydrate((value as any)[k]);
    }
    return hydrated[index];
  }
  return hydrate(0);
}

/** Decode a WDK input/output blob to a JS value (best-effort). */
function decodeBlob(v: any): any {
  if (v == null) return v;
  if (typeof v !== "string") return v; // already decoded
  try {
    const s = typeof atob === "function" ? atob(v) : v;
    if (s.startsWith("devl")) return unflatten(JSON.parse(s.slice(4)));
    try { return JSON.parse(s); } catch { return s; }
  } catch {
    return v;
  }
}

function pretty(v: any): string {
  try {
    return JSON.stringify(v, (_k, val) => (val === undefined ? "__undefined__" : val), 2)
      .replace(/"__undefined__"/g, "undefined");
  } catch {
    return String(v);
  }
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

const STATUS_META: Record<string, { label: string; dot: string; bar: string; text: string }> = {
  succeeded: { label: "Completed", dot: "bg-emerald-500", bar: "bg-emerald-500/70 border-emerald-500", text: "text-emerald-600 dark:text-emerald-400" },
  completed: { label: "Completed", dot: "bg-emerald-500", bar: "bg-emerald-500/70 border-emerald-500", text: "text-emerald-600 dark:text-emerald-400" },
  running: { label: "Active", dot: "bg-amber-500", bar: "bg-amber-400/70 border-amber-500", text: "text-amber-600 dark:text-amber-400" },
  failed: { label: "Failed", dot: "bg-red-500", bar: "bg-red-500/70 border-red-500", text: "text-red-600 dark:text-red-400" },
  error: { label: "Failed", dot: "bg-red-500", bar: "bg-red-500/70 border-red-500", text: "text-red-600 dark:text-red-400" },
  cancelled: { label: "Cancelled", dot: "bg-zinc-400", bar: "bg-zinc-400/60 border-zinc-400", text: "text-secondary" },
  pending: { label: "Pending", dot: "bg-zinc-400", bar: "bg-zinc-300/60 border-zinc-400", text: "text-secondary" },
  sleep: { label: "Sleep", dot: "bg-zinc-400", bar: "bg-zinc-300/70 border-zinc-400", text: "text-secondary" },
};
function meta(s: string) {
  return STATUS_META[(s || "").toLowerCase()] || STATUS_META.pending;
}

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
  const tab = (sp.get("tab") as "trace" | "events") === "events" ? "events" : "trace";
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

  const setTab = (t: "trace" | "events") => {
    const q = new URLSearchParams(sp.toString());
    q.set("tab", t);
    router.replace(`${pathname}?${q.toString()}`);
  };

  const steps = useMemo(() => d?.steps || [], [d]);
  const events = useMemo(() => d?.events || [], [d]);

  const spans = useMemo<Span[]>(() => {
    const out: Span[] = [];
    for (const s of steps) {
      const start = ms(s.startedAt) ?? ms(s.createdAt);
      if (start == null) continue;
      const end = ms(s.completedAt) ?? (isLive ? now : start);
      const { module, name } = parseName(s.stepName);
      out.push({
        key: `step:${s.stepId}`, kind: "step", name, module, start, end,
        status: String(s.status || (s.completedAt ? "completed" : "running")),
        attempt: s.attempt, raw: s, cid: s.stepId,
      });
    }
    const waits = events
      .filter((e) => e.eventType === "wait_created" || e.eventType === "wait_completed")
      .sort((a, b) => (ms(a.createdAt) ?? 0) - (ms(b.createdAt) ?? 0));
    let pending: Record<string, any> | null = null;
    let wi = 0;
    for (const e of waits) {
      if (e.eventType === "wait_created") pending = e;
      else if (e.eventType === "wait_completed" && pending) {
        const start = ms(pending.createdAt);
        if (start != null) {
          const end = ms(e.createdAt) ?? ms(pending.eventData?.resumeAt) ?? start;
          out.push({ key: `wait:${wi++}`, kind: "sleep", name: "sleep", module: "", start, end, status: "sleep", raw: { ...pending, completed: e }, cid: pending.eventId });
        }
        pending = null;
      }
    }
    if (pending) {
      const start = ms(pending.createdAt);
      if (start != null) {
        const end = ms(pending.eventData?.resumeAt) ?? (isLive ? now : start);
        out.push({ key: `wait:${wi++}`, kind: "sleep", name: "sleep", module: "", start, end, status: "sleep", raw: pending, cid: pending.eventId });
      }
    }
    out.sort((a, b) => a.start - b.start);
    return out;
  }, [steps, events, isLive, now]);

  const rootStart = ms(run?.createdAt) ?? ms(run?.startedAt) ?? (spans[0]?.start ?? now);
  const rootEnd = ms(run?.completedAt) ?? (isLive ? now : Math.max(rootStart, ...spans.map((s) => s.end)));
  const rootSpan: Span | null = run
    ? { key: "root", kind: "root", ...parseName(run.workflowName), start: rootStart, end: rootEnd, status, raw: run, cid: run.runId }
    : null;

  const t0 = rootStart;
  const t1 = Math.max(rootEnd, ...spans.map((s) => s.end), t0 + 1);
  const selected: Span | null = sel === "root" ? rootSpan : spans.find((s) => s.key === sel) || null;
  const wfName = parseName(run?.workflowName).name || id;

  return (
    <div className="pb-10">
      <div className="mb-4 flex items-center gap-2 text-sm text-secondary">
        <Link href="/workflows" className="hover:text-fg">Workflows</Link>
        <span className="text-muted">/</span>
        <span className="font-mono text-fg">{id}</span>
      </div>

      <div className="mb-5 flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="text-xl font-semibold tracking-tight">{wfName}</h1>
          <CopyText text={id} className="font-mono text-sm text-secondary" />
          <span className={`flex items-center gap-1.5 text-sm ${meta(status).text}`}>
            <span className={`h-2 w-2 rounded-full ${meta(status).dot} ${isLive ? "animate-pulse" : ""}`} />
            {isLive ? "Active" : meta(status).label}
          </span>
        </div>
        {project && (
          <Link href={`/projects/${encodeURIComponent(project)}?tab=workflows`} className="inline-flex items-center gap-1 text-sm text-link hover:underline">
            {project} <ExternalLink className="h-3 w-3" />
          </Link>
        )}
      </div>

      <div className="mb-5 grid grid-cols-2 gap-x-8 gap-y-3 sm:grid-cols-5">
        <Stat label="Created" value={run?.createdAt ? new Date(ms(run.createdAt)!).toLocaleTimeString() : "—"} />
        <Stat label="Completed" value={run?.completedAt ? new Date(ms(run.completedAt)!).toLocaleTimeString() : "—"} />
        <Stat label="Duration" value={run ? fmtDur(rootEnd - rootStart) : "—"} />
        <Stat label="Steps" value={String(steps.length)} />
        <Stat label="Spec" value={run?.specVersion != null ? String(run.specVersion) : "—"} />
      </div>

      <div className="mb-4 flex items-center gap-1">
        {(["trace", "events"] as const).map((t) => (
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

      {tab === "trace" ? (
        <TraceView rootSpan={rootSpan} spans={spans} t0={t0} t1={t1} selectedKey={sel} onSelect={setSel} selected={selected} events={events} />
      ) : (
        <EventsView events={events} steps={steps} runId={id} />
      )}
    </div>
  );
}

function TraceView({
  rootSpan, spans, t0, t1, selectedKey, onSelect, selected, events,
}: {
  rootSpan: Span | null; spans: Span[]; t0: number; t1: number;
  selectedKey: string | null; onSelect: (k: string | null) => void; selected: Span | null;
  events: Record<string, any>[];
}) {
  const total = Math.max(1, t1 - t0);
  const ticks = useMemo(() => Array.from({ length: 6 }, (_, i) => t0 + (total * i) / 5), [t0, total]);

  const Row = ({ s, depth }: { s: Span; depth: number }) => {
    const leftPct = ((s.start - t0) / total) * 100;
    const widthPct = Math.max(0.6, ((s.end - s.start) / total) * 100);
    const m = meta(s.status);
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
          <span className="ml-auto shrink-0 pl-2 tabular-nums text-xs text-muted">{fmtDur(s.end - s.start)}</span>
        </span>
        <span className="relative h-9 border-l border-border">
          <span
            className={`absolute top-1/2 h-3 -translate-y-1/2 rounded-sm border ${m.bar}`}
            style={{ left: `${leftPct}%`, width: `${widthPct}%`, minWidth: 2 }}
            title={`${s.name} · ${fmtDur(s.end - s.start)}`}
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
                {fmtDur(t - t0)}
              </span>
            ))}
          </span>
        </div>
        {rootSpan && <Row s={rootSpan} depth={0} />}
        {spans.map((s) => <Row key={s.key} s={s} depth={1} />)}
        {spans.length === 0 && !rootSpan && (
          <div className="px-4 py-12 text-center text-sm text-secondary">No spans recorded for this run.</div>
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
        {span.module && <Field label="Module" mono>{span.module}</Field>}
        {span.kind === "step" && <Field label="Step ID" mono copy>{raw.stepId}</Field>}
        {span.kind === "step" && <Field label="Attempts">{String(raw.attempt ?? 1)}</Field>}
        {raw.specVersion != null && <Field label="Spec Version">{String(raw.specVersion)}</Field>}
        <Field label="Created">{tsStr(raw.createdAt)}</Field>
        {span.kind !== "sleep" && <Field label="Started">{tsStr(raw.startedAt)}</Field>}
        <Field label="Completed">{span.kind === "sleep" ? tsStr(raw.completed?.createdAt) : tsStr(raw.completedAt)}</Field>
        {span.kind === "sleep" && <Field label="Resume At">{tsStr(raw.eventData?.resumeAt)}</Field>}
        {raw.error ? <Field label="Error" mono>{String(raw.error)}</Field> : null}

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
    const r = [...events].sort((a, b) => (ms(a.createdAt) ?? 0) - (ms(b.createdAt) ?? 0));
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
        <div className="px-4 py-12 text-center text-sm text-secondary">No events.</div>
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
  const m = ms(t);
  return m ? new Date(m).toLocaleString() : "—";
}
function timeStr(t: unknown): string {
  const m = ms(t);
  if (!m) return "—";
  const dt = new Date(m);
  return `${dt.toLocaleTimeString([], { hour12: false })}.${String(dt.getMilliseconds()).padStart(3, "0")}`;
}
function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-xs text-muted">{label}</div>
      <div className="text-sm tabular-nums">{value}</div>
    </div>
  );
}
function Field({ label, children, mono, copy }: { label: string; children: React.ReactNode; mono?: boolean; copy?: boolean }) {
  return (
    <div className="mb-2 grid grid-cols-[100px_1fr] items-start gap-2">
      <span className="text-xs text-muted">{label}</span>
      {copy ? <CopyText text={String(children)} className={`break-all ${mono ? "font-mono text-xs" : ""}`} />
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
function CopyText({ text, className }: { text: string; className?: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      onClick={() => { navigator.clipboard?.writeText(text); setDone(true); setTimeout(() => setDone(false), 1200); }}
      className={`inline-flex items-center gap-1 hover:text-fg ${className || ""}`}
      title="Copy"
    >
      <span className="truncate">{text}</span>
      {done ? <Check className="h-3 w-3 shrink-0 text-emerald-500" /> : <Copy className="h-3 w-3 shrink-0 text-muted" />}
    </button>
  );
}
