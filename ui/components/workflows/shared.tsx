"use client";

// ---------------------------------------------------------------------------
// Shared workflow-console primitives — a faithful port of the Vercel
// @workflow/web + @workflow/web-shared observability console, reimplemented
// natively on hive's design tokens (so it embeds in the dashboard chrome,
// "minus the navbar") and wired to hive's /v1/workflows/* data plane.
//
// Status vocabulary, atoms (RelativeTime / CopyableText / LiveStatus /
// StatusBadge), filters, skeletons, empty-states, name parsing, and the WDK
// devalue+CBOR decode helpers all live here so the three surfaces — global
// /workflows, per-project ?tab=workflows, and /workflows/runs/[id] — are one
// component library.
// ---------------------------------------------------------------------------

import { useEffect, useMemo, useRef, useState } from "react";
import { Check, Copy, ChevronDown, Loader2, Search } from "lucide-react";
import { cn, timeAgo, copyText } from "@/lib/utils";
import type { RunStatus, WorkflowRun } from "@/lib/api";

/* ---- status vocabulary (upstream: pending/running/completed/failed/cancelled) ---- */

export type WfStatus = "pending" | "running" | "completed" | "failed" | "cancelled";

export interface StatusMeta {
  /** Upstream label surfaced to the user. */
  label: string;
  /** Dot / accent color class. */
  dot: string;
  /** Text color class. */
  text: string;
  /** Trace-bar fill+border classes. */
  bar: string;
}

const STATUS_META: Record<WfStatus, StatusMeta> = {
  completed: { label: "Completed", dot: "bg-emerald-500", text: "text-emerald-600 dark:text-emerald-400", bar: "bg-emerald-500/70 border-emerald-500" },
  running: { label: "Running", dot: "bg-blue-500", text: "text-blue-600 dark:text-blue-400", bar: "bg-blue-400/70 border-blue-500" },
  failed: { label: "Failed", dot: "bg-red-500", text: "text-red-600 dark:text-red-400", bar: "bg-red-500/70 border-red-500" },
  pending: { label: "Pending", dot: "bg-zinc-400", text: "text-secondary", bar: "bg-zinc-300/60 border-zinc-400" },
  cancelled: { label: "Cancelled", dot: "bg-zinc-400", text: "text-secondary", bar: "bg-zinc-400/60 border-zinc-400" },
};

/** Normalize any status spelling (hive internal `succeeded`, WDK `completed`,
 *  `error`, casing) to the upstream WfStatus vocabulary. */
export function normStatus(s: string | undefined | null): WfStatus {
  switch ((s || "").toLowerCase()) {
    case "succeeded":
    case "completed":
      return "completed";
    case "running":
    case "active":
      return "running";
    case "failed":
    case "error":
    case "errored":
      return "failed";
    case "cancelled":
    case "canceled":
      return "cancelled";
    default:
      return "pending";
  }
}

export function statusMeta(s: string | undefined | null): StatusMeta {
  return STATUS_META[normStatus(s)];
}

/** Faithful StatusBadge — a colored dot + upstream label ("Running", not
 *  "Active"). A running status pulses the dot. */
export function StatusBadge({ status, live, className }: { status: string | undefined | null; live?: boolean; className?: string }) {
  const m = statusMeta(status);
  const isRunning = normStatus(status) === "running";
  return (
    <span className={cn("inline-flex items-center gap-1.5 text-sm", m.text, className)}>
      <span className={cn("h-2 w-2 rounded-full", m.dot, (live || isRunning) && "animate-pulse")} />
      {m.label}
    </span>
  );
}

/** A "running" run with no world activity for this long is presumed stalled —
 *  its ticking duration is annotated so it doesn't read as a healthy live span. */
export const STALE_RUNNING_MS = 6 * 3600_000;

export function isStaleRunning(r: WorkflowRun, now: number): boolean {
  if (normStatus(r.status) !== "running") return false;
  const lastActivity = toMs((r as any).updatedAt) ?? r.started_ms;
  return !!lastActivity && now - lastActivity > STALE_RUNNING_MS;
}

/* ---- duration ---- */

export function fmtDuration(ms: number): string {
  if (!isFinite(ms) || ms < 0) ms = 0;
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(2).replace(/\.?0+$/, "")}s`;
  const m = Math.floor(s / 60);
  const rem = Math.round(s % 60);
  if (m < 60) return `${m}m ${rem}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

export function runDuration(r: WorkflowRun, now: number): number {
  const end = r.finished_ms ?? now;
  return Math.max(0, end - r.started_ms);
}

/* ---- time ---- */

/** Relative time with a SINGLE "ago" suffix (fixes the "22h ago ago" bug that
 *  came from appending " ago" to timeAgo(), which already includes it), plus an
 *  absolute-time tooltip. Renders "—" for missing/zero. */
export function RelativeTime({ ms, className }: { ms?: number | null; className?: string }) {
  if (!ms || ms <= 0) return <span className={cn("text-muted", className)}>—</span>;
  return (
    <time dateTime={new Date(ms).toISOString()} title={new Date(ms).toLocaleString()} className={className}>
      {timeAgo(ms)}
    </time>
  );
}

/** Absolute local time (e.g. for the stat row). */
export function AbsTime({ ms, className }: { ms?: number | null; className?: string }) {
  if (!ms || ms <= 0) return <span className={cn("text-muted", className)}>—</span>;
  return (
    <time dateTime={new Date(ms).toISOString()} title={new Date(ms).toLocaleString()} className={className}>
      {new Date(ms).toLocaleString([], { hour12: false })}
    </time>
  );
}

/** ms since epoch (accepts epoch-seconds float or ms). null for missing. */
export function toMs(t: unknown): number | null {
  if (typeof t !== "number" || t <= 0) return null;
  return t > 1e12 ? t : Math.round(t * 1000);
}

/* ---- copyable mono id ---- */

export function CopyableText({ text, className, mono = true, truncate }: { text: string; className?: string; mono?: boolean; truncate?: boolean }) {
  const [done, setDone] = useState(false);
  return (
    <button
      type="button"
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        copyText(text).then(() => {
          setDone(true);
          setTimeout(() => setDone(false), 1200);
        });
      }}
      title="Copy"
      className={cn("group inline-flex max-w-full items-center gap-1 hover:text-fg", mono && "font-mono", className)}
    >
      <span className={cn(truncate && "truncate")}>{text}</span>
      {done ? <Check className="h-3 w-3 shrink-0 text-emerald-500" /> : <Copy className="h-3 w-3 shrink-0 text-muted opacity-0 transition-opacity group-hover:opacity-100" />}
    </button>
  );
}

/* ---- live status (pulsing) ---- */

export function LiveStatus({ status, live }: { status: string | undefined | null; live?: boolean }) {
  return <StatusBadge status={status} live={live} />;
}

/* ---- filter select (faithful dropdown pill) ---- */

export function FilterSelect({
  label,
  value,
  options,
  onChange,
  icon,
}: {
  label?: string;
  value: string;
  options: string[] | { value: string; label: string }[];
  onChange: (v: string) => void;
  icon?: React.ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const opts = options.map((o) => (typeof o === "string" ? { value: o, label: o } : o));
  const current = opts.find((o) => o.value === value)?.label ?? value;
  return (
    <div className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        onBlur={() => setTimeout(() => setOpen(false), 150)}
        className="flex items-center gap-2 rounded-md border border-border-strong px-3 py-1.5 text-sm text-fg hover:bg-subtle"
      >
        {icon}
        {label && <span className="text-muted">{label}</span>}
        {current}
        <ChevronDown className="h-3.5 w-3.5 text-muted" />
      </button>
      {open && (
        <div className="absolute left-0 z-30 mt-1 max-h-72 w-48 overflow-auto rounded-lg border border-border bg-card py-1 shadow-pop">
          {opts.map((o) => (
            <button
              key={o.value}
              type="button"
              onMouseDown={() => {
                onChange(o.value);
                setOpen(false);
              }}
              className={cn("block w-full px-3 py-1.5 text-left text-sm hover:bg-subtle", o.value === value && "font-medium text-fg")}
            >
              {o.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/** Search input with a leading magnifier — the console's list filter. */
export function SearchInput({ value, onChange, placeholder }: { value: string; onChange: (v: string) => void; placeholder?: string }) {
  return (
    <div className="relative flex-1 min-w-[180px]">
      <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
      <input
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder || "Search…"}
        className="w-full rounded-md border border-border bg-card py-2 pl-9 pr-3 text-sm text-fg placeholder:text-muted focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border"
      />
    </div>
  );
}

/* ---- skeleton + empty ---- */

export function WfTableSkeleton({ rows = 6, cols = 5 }: { rows?: number; cols?: number }) {
  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card">
      <div className="flex items-center gap-4 border-b border-border bg-subtle/40 px-4 py-2.5">
        {Array.from({ length: cols }).map((_, i) => (
          <div key={i} className="h-3 flex-1 animate-pulse rounded bg-subtle" />
        ))}
      </div>
      {Array.from({ length: rows }).map((_, i) => (
        <div key={i} className="flex items-center gap-4 border-b border-border px-4 py-3.5 last:border-0">
          {Array.from({ length: cols }).map((_, j) => (
            <div key={j} className="h-4 flex-1 animate-pulse rounded bg-subtle" />
          ))}
        </div>
      ))}
    </div>
  );
}

export function EmptyState({ title, hint, icon }: { title: string; hint?: string; icon?: React.ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 px-4 py-16 text-center">
      {icon && <div className="text-muted">{icon}</div>}
      <div className="text-sm font-medium text-secondary">{title}</div>
      {hint && <div className="max-w-md text-xs text-muted">{hint}</div>}
    </div>
  );
}

/* ---- time ranges (upstream vocabulary) ---- */

export const TIME_RANGES: Record<string, number> = {
  "Last hour": 3600_000,
  "Last 6 hours": 6 * 3600_000,
  "Last 24 hours": 24 * 3600_000,
  "Last 3 days": 3 * 24 * 3600_000,
  "Last 7 days": 7 * 24 * 3600_000,
  "Last 30 days": 30 * 24 * 3600_000,
  "All time": Number.POSITIVE_INFINITY,
};
/** Default window wide enough that a project whose newest run is a day+ old
 *  still shows activity (the "empty /workflows" bug was a 12h default hiding
 *  runs that were ~22h old). */
export const DEFAULT_RANGE = "Last 7 days";

/* ---- name parsing (WDK ids) ---- */

/** "workflow//./app/workflows/session//sessionWorkflow" → "sessionWorkflow". */
export function cleanWfName(n?: string): string {
  if (!n) return "workflow";
  const parts = n.split("/").filter(Boolean);
  return parts[parts.length - 1] || n;
}

/** "step//./app/steps/autopilotSteps//listProactiveTenantsStep" →
 *  { module: "./app/steps/autopilotSteps", name: "listProactiveTenantsStep" }. */
export function parseName(raw?: string): { module: string; name: string } {
  if (!raw) return { module: "", name: "" };
  const parts = raw.split("//").map((s) => s.trim()).filter(Boolean);
  if (parts.length >= 3) return { module: parts[1], name: parts[parts.length - 1] };
  if (parts.length === 2) return { module: parts[0], name: parts[1] };
  return { module: "", name: parts[parts.length - 1] || raw };
}

/** The source file a workflow/step id came from ("./app/workflows/session"). */
export function parseFile(raw?: string): string {
  if (!raw) return "";
  const parts = raw.split("//").map((s) => s.trim()).filter(Boolean);
  if (parts.length >= 2) return parts[1];
  return "";
}

/* ---- WDK run normalization ---- *
 * Runs read from a deployed app's WDK world use the native shape (runId /
 * workflowName / status: completed / epoch-seconds timestamps). The backend
 * enrich_run already supersets the internal fields, but we normalize defensively
 * so a raw WDK row also renders. */
export function normalizeRun(r: any): WorkflowRun {
  const wdk = r && (r.runId !== undefined || r.workflowName !== undefined || r.startedAt !== undefined);
  if (!wdk) return r as WorkflowRun;
  return {
    id: r.id ?? r.runId ?? "",
    def_id: r.def_id ?? r.workflowName ?? "",
    name: r.name ?? cleanWfName(r.workflowName ?? r.name),
    project: r.project ?? "",
    status: mapToInternal(r.status),
    steps: Array.isArray(r.steps) ? r.steps : [],
    started_ms: r.started_ms ?? toMs(r.startedAt) ?? toMs(r.createdAt) ?? 0,
    finished_ms: r.finished_ms ?? toMs(r.completedAt) ?? null,
    environment: r.environment ?? "production",
    createdAt: r.createdAt,
    updatedAt: r.updatedAt,
    workflowName: r.workflowName,
  } as WorkflowRun;
}

/** WDK/upstream status → hive internal RunStatus (succeeded, not completed). */
function mapToInternal(s?: string): RunStatus {
  switch ((s || "").toLowerCase()) {
    case "completed":
    case "succeeded":
      return "succeeded";
    case "running":
      return "running";
    case "failed":
    case "error":
      return "failed";
    case "cancelled":
      return "cancelled";
    default:
      return "pending";
  }
}

/* ---- devalue + base64 decode (WDK encodes input/output as
 *  base64("devl" + devalue.stringify(v)) with encryption off) ---- */

export function unflatten(parsed: any): any {
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

export function decodeBlob(v: any): any {
  if (v == null) return v;
  if (typeof v !== "string") return v;
  try {
    const s = typeof atob === "function" ? atob(v) : v;
    if (s.startsWith("devl")) return unflatten(JSON.parse(s.slice(4)));
    try {
      return JSON.parse(s);
    } catch {
      return s;
    }
  } catch {
    return v;
  }
}

export function pretty(v: any): string {
  try {
    return JSON.stringify(v, (_k, val) => (val === undefined ? "__undefined__" : val), 2).replace(/"__undefined__"/g, "undefined");
  } catch {
    return String(v);
  }
}

/* ---- ticking clock hook (only re-renders while something is live) ---- */
export function useNow(ms = 1000) {
  const [now, setNow] = useState(() => Date.now());
  const ref = useRef(ms);
  ref.current = ms;
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(id);
  }, [ms]);
  return now;
}
