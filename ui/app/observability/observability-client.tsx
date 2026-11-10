"use client";

import { useState } from "react";
import Link from "next/link";
import dynamic from "next/dynamic";
import { Calendar, ChevronDown, Search, MoreHorizontal, ChevronRight } from "lucide-react";

const AreaChart = dynamic(() => import("@tremor/react").then((m) => m.AreaChart), { ssr: false });
import { Card } from "@/components/ui";
import { usePoll, type Metrics, type RumSummary } from "@/lib/api";
import { SpeedInsights } from "@/components/speed-insights";
import { cn } from "@/lib/utils";

const RANGES = [
  { m: 60, label: "Last 1 hour" },
  { m: 360, label: "Last 6 hours" },
  { m: 720, label: "Last 12 hours" },
];
const DEFAULT_RANGE_MINUTES = 720;

const BAR_COLORS = ["#0070f3", "#f5a623", "#7928ca", "#50e3c2", "#e00", "#f81ce5", "#0070f3", "#79ffe1"];

const ABSENT = "-";

function measured(v: number | null | undefined): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function sumMeasured<T>(rows: T[] | null | undefined, pick: (row: T) => number | null | undefined): number | null {
  if (!Array.isArray(rows)) return null;
  let total = 0;
  for (const row of rows) {
    const value = measured(pick(row));
    if (value === null) return null;
    total += value;
  }
  return total;
}

function fmtNum(n: number | null): string {
  const value = measured(n);
  if (value === null) return ABSENT;
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(value >= 10_000 ? 0 : 1)}K`;
  return `${value}`;
}

export function ObservabilityClient({
  initialTab,
  initialMetrics,
  initialSpeedInsights,
}: {
  initialTab: "overview" | "speed";
  initialMetrics: Metrics | null;
  initialSpeedInsights: RumSummary | null;
}) {
  const [range, setRange] = useState(DEFAULT_RANGE_MINUTES);
  const [q, setQ] = useState("");
  const [tab, setTab] = useState<"overview" | "speed">(initialTab);
  const { data } = usePoll<Metrics>(`/v1/metrics?minutes=${range}`, 5000, true, initialMetrics);
  const rangeLabel = RANGES.find((r) => r.m === range)?.label ?? "Last 12 hours";

  const tabBar = (
    <div className="mb-6 flex min-w-0 w-full max-w-full flex-wrap gap-5 border-b border-border text-sm">
      {([["overview", "Overview"], ["speed", "Speed Insights"]] as const).map(([id, label]) => (
        <button
          key={id}
          onClick={() => {
            setTab(id);
            const u = new URL(window.location.href);
            if (id === "speed") u.searchParams.set("tab", "speed-insights");
            else u.searchParams.delete("tab");
            window.history.replaceState({}, "", u.toString());
          }}
          className={cn(
            "-mb-px min-w-0 break-words border-b-2 pb-3 transition-colors",
            tab === id ? "border-fg font-medium text-fg" : "border-transparent text-secondary hover:text-fg"
          )}
        >
          {label}
        </button>
      ))}
    </div>
  );

  if (tab === "speed") {
    return (
      <div className="min-w-0 w-full max-w-full">
        {tabBar}
        <div className="min-w-0 w-full max-w-full overflow-x-auto">
          <SpeedInsights initial={initialSpeedInsights} />
        </div>
      </div>
    );
  }

  const series = data?.series ?? [];
  const reqSeries = series.map((b) => ({
    t: new Date(b.t_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
    Requests: b.requests,
    Errors: b.errors + b.client_err,
  }));
  const cacheSeries = series.map((b) => ({
    t: new Date(b.t_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
    Hits: b.cache_hits,
    Misses: b.cache_miss,
  }));
  const mwSeries = series.map((b) => ({
    t: new Date(b.t_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
    Invocations: b.blocked,
  }));

  const totalReq = measured(data?.totals?.requests);
  const cacheHitRatio = measured(data?.totals?.cache_hit_ratio);
  const cacheHitPct = cacheHitRatio === null ? null : cacheHitRatio * 100;
  const totalMw = sumMeasured(data?.series, (b) => b.blocked);

  const projectRows = data?.projects;
  const projectsReady = Array.isArray(projectRows);
  const projects = (projectRows ?? []).filter((p) =>
    p.project.toLowerCase().includes(q.toLowerCase())
  );
  const projectCount = projectsReady ? projects.length : null;

  return (
    <div className="min-w-0 w-full max-w-full">
      {tabBar}
      <div className="mb-6 flex min-w-0 w-full max-w-full flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <h1 className="min-w-0 break-words text-lg font-semibold">Observability</h1>
        <div className="flex min-w-0 w-full max-w-full flex-wrap items-center gap-2">
          <button className="flex min-w-0 w-full max-w-full items-center gap-2 rounded-md border border-border bg-card px-3 py-1.5 text-sm">
            <span className="min-w-0 break-words">Production</span>
            <ChevronDown className="h-3.5 w-3.5 shrink-0 text-muted" />
          </button>
          <div className="relative min-w-0 w-full max-w-full">
            <select
              value={range}
              onChange={(e) => setRange(Number(e.target.value))}
              className="w-full max-w-full appearance-none rounded-md border border-border bg-card py-1.5 pl-9 pr-8 text-sm"
            >
              {RANGES.map((r) => <option key={r.m} value={r.m}>{r.label}</option>)}
            </select>
            <Calendar className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
            <ChevronDown className="pointer-events-none absolute right-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted" />
          </div>
          <button className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md border border-border text-muted hover:bg-subtle">
            <MoreHorizontal className="h-4 w-4 shrink-0" />
          </button>
        </div>
      </div>

      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <MetricCard title="Edge Requests" metric="Invocations" value={fmtNum(totalReq)} href="/observability"
          data={reqSeries} categories={["Requests", "Errors"]} colors={["blue", "amber"]} label={rangeLabel} />
        <MetricCard title="Cache Hit Ratio" metric="Hits vs Misses" value={cacheHitPct === null ? ABSENT : `${cacheHitPct.toFixed(1)}%`} href="/observability"
          data={cacheSeries} categories={["Hits", "Misses"]} colors={["blue", "amber"]} label={rangeLabel} />
        <MetricCard title="Functions" metric="Invocations" value={fmtNum(totalReq)} href="/observability"
          data={reqSeries} categories={["Requests"]} colors={["blue"]} label={rangeLabel} />
        <MetricCard title="Middleware Invocations" metric="Invocations" value={fmtNum(totalMw)} href="/firewall"
          data={mwSeries} categories={["Invocations"]} colors={["amber"]} label={rangeLabel} />
      </div>

      <div className="mt-6 min-w-0 w-full max-w-full">
        <div className="relative mb-3 min-w-0 w-full max-w-full">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Search…"
            className="w-full max-w-full rounded-lg border border-border bg-card py-2.5 pl-9 pr-3 text-sm focus:outline-none focus:ring-2 focus:ring-border"
          />
        </div>
        <Card className="min-w-0 w-full max-w-full p-0">
          <div className="min-w-0 w-full max-w-full overflow-x-auto">
            <div className="flex min-w-0 w-full max-w-full items-center justify-between gap-3 border-b border-border px-4 py-2.5 text-xs font-medium text-muted">
              <span className="min-w-0 break-words">Project</span>
              <span className="flex shrink-0 items-center gap-1">Requests <ChevronDown className="h-3 w-3 shrink-0" /></span>
            </div>
            {projects.map((p, i) => (
              <Link
                key={p.project}
                href={`/projects/${encodeURIComponent(p.project)}`}
                className="flex min-w-0 w-full max-w-full items-center justify-between gap-3 border-b border-border px-4 py-3 text-sm last:border-0 hover:bg-subtle"
              >
                <span className="flex min-w-0 items-center gap-3">
                  <span className="h-4 w-1 shrink-0 rounded-full" style={{ background: BAR_COLORS[i % BAR_COLORS.length] }} />
                  <span className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-fg text-bg">
                    <svg width="9" height="8" viewBox="0 0 24 22" aria-hidden><path d="M12 0 L24 22 L0 22 Z" fill="currentColor" /></svg>
                  </span>
                  <span className="min-w-0 break-words font-medium">{p.project}</span>
                </span>
                <span className="flex shrink-0 items-center gap-3">
                  <span className="min-w-0 break-words rounded-md bg-subtle px-2 py-0.5 font-medium tabular-nums">{fmtNum(p.requests)}</span>
                  <ChevronRight className="h-4 w-4 shrink-0 text-muted" />
                </span>
              </Link>
            ))}
            {!projectsReady ? (
              <div className="min-w-0 break-words px-4 py-10 text-center text-sm text-secondary">Traffic data is unavailable for this session.</div>
            ) : !projects.length ? (
              <div className="min-w-0 break-words px-4 py-10 text-center text-sm text-secondary">No projects match this search.</div>
            ) : null}
          </div>
        </Card>
        <div className="mt-3 flex min-w-0 w-full max-w-full flex-wrap items-center justify-between gap-2 text-xs text-muted">
          <span>Matching projects: {projectCount === null ? ABSENT : projectCount}</span>
          <span>{rangeLabel}</span>
        </div>
      </div>
    </div>
  );
}

function MetricCard({
  title, metric, value, data, categories, colors, label, href,
}: {
  title: string; metric: string; value: string;
  data: Record<string, unknown>[]; categories: string[]; colors: string[]; label: string; href: string;
}) {
  return (
    <Card className="min-w-0 w-full max-w-full p-5">
      <Link href={href} className="mb-3 flex min-w-0 w-full max-w-full items-center justify-between gap-3">
        <span className="min-w-0 break-words text-sm font-semibold">{title}</span>
        <ChevronRight className="h-4 w-4 shrink-0 text-muted" />
      </Link>
      <div className="mb-1 min-w-0 break-words text-xs text-secondary">{metric}</div>
      <div className="mb-3 min-w-0 break-words text-xl font-semibold tabular-nums whitespace-normal sm:text-2xl">{value}</div>
      <div className="min-w-0 w-full max-w-full overflow-x-auto">
        <AreaChart
          className="h-40 w-full max-w-full"
          data={data}
          index="t"
          categories={categories}
          colors={colors as any}
          showLegend={false}
          showGridLines={false}
          showXAxis={false}
          showYAxis
          yAxisWidth={36}
          curveType="monotone"
          startEndOnly
        />
      </div>
      <div className="mt-1 flex min-w-0 w-full max-w-full flex-wrap justify-between gap-2 break-words text-[11px] text-muted">
        <span>{label.replace("Last ", "")} ago</span>
        <span>now</span>
      </div>
    </Card>
  );
}
