"use client";

import { useState } from "react";
import Link from "next/link";
import dynamic from "next/dynamic";
import { Calendar, ChevronDown, Search, MoreHorizontal, ChevronRight } from "lucide-react";

// Lazy-load the Tremor chart bundle so it isn't in the initial page payload.
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
// Must match page.tsx's server prefetch default (fetchMetricsSummary(team, 720)).
const DEFAULT_RANGE_MINUTES = 720;

const BAR_COLORS = ["#0070f3", "#f5a623", "#7928ca", "#50e3c2", "#e00", "#f81ce5", "#0070f3", "#79ffe1"];

function fmtNum(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(n >= 10_000 ? 0 : 1)}K`;
  return `${n}`;
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
  // `initialTab` is computed SERVER-SIDE from the URL's `?tab=` query param
  // (page.tsx) and passed as a prop, so this useState is a plain literal
  // initializer instead of a function that reads `window.location.search`.
  //
  // The old code's initializer function ran on BOTH the server render (where
  // `window` is undefined -> always "overview") and the client's hydration
  // render (where, for a URL like ?tab=speed-insights, it read the real
  // location and returned "speed") -- a genuine server/client mismatch on
  // every direct link or reload with that query param, the same class of bug
  // commit b563dac fixed elsewhere in this app (React #418 hydration crash).
  // Server and client now agree by construction: the value can no longer be
  // constructed two different ways depending on which environment computes it.
  const [tab, setTab] = useState<"overview" | "speed">(initialTab);
  const { data } = usePoll<Metrics>(`/v1/metrics?minutes=${range}`, 5000, true, initialMetrics);
  const rangeLabel = RANGES.find((r) => r.m === range)?.label ?? "Last 12 hours";

  const tabBar = (
    <div className="mb-6 flex gap-5 border-b border-border text-sm">
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
            "-mb-px border-b-2 pb-3 transition-colors",
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
      <div>
        {tabBar}
        <SpeedInsights initial={initialSpeedInsights} />
      </div>
    );
  }

  const series = data?.series ?? [];
  const reqSeries = series.map((b) => ({
    t: new Date(b.t_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
    Requests: b.requests,
    Errors: b.errors + b.client_err,
  }));
  // REAL cache hit/miss counts (metrics.rs's Bucket tracks these exactly —
  // no fabrication). Replaces a previous "Fast Data Transfer" card that
  // multiplied request count by a hardcoded ~4.3KB/request guess: this crate
  // has no bytes-transferred instrumentation anywhere (Event/Bucket carry no
  // such field), so that number was invented, not measured — matching the
  // user-reported "inaccurate" observability data. Cache hit ratio is exact.
  const cacheSeries = series.map((b) => ({
    t: new Date(b.t_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
    Hits: b.cache_hits,
    Misses: b.cache_miss,
  }));
  const mwSeries = series.map((b) => ({
    t: new Date(b.t_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
    Invocations: b.blocked,
  }));

  const totalReq = data?.totals?.requests ?? 0;
  const cacheHitPct = (data?.totals?.cache_hit_ratio ?? 0) * 100;
  const totalMw = series.reduce((a, b) => a + b.blocked, 0);

  const projects = (data?.projects ?? []).filter((p) =>
    p.project.toLowerCase().includes(q.toLowerCase())
  );

  return (
    <div>
      {tabBar}
      {/* Top controls */}
      <div className="mb-6 flex items-center justify-between">
        <h1 className="text-lg font-semibold">Observability</h1>
        <div className="flex items-center gap-2">
          <button className="flex items-center gap-2 rounded-md border border-border bg-card px-3 py-1.5 text-sm">
            Production <ChevronDown className="h-3.5 w-3.5 text-muted" />
          </button>
          <div className="relative">
            <select
              value={range}
              onChange={(e) => setRange(Number(e.target.value))}
              className="appearance-none rounded-md border border-border bg-card py-1.5 pl-9 pr-8 text-sm"
            >
              {RANGES.map((r) => <option key={r.m} value={r.m}>{r.label}</option>)}
            </select>
            <Calendar className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
            <ChevronDown className="pointer-events-none absolute right-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted" />
          </div>
          <button className="flex h-8 w-8 items-center justify-center rounded-md border border-border text-muted hover:bg-subtle">
            <MoreHorizontal className="h-4 w-4" />
          </button>
        </div>
      </div>

      {/* Metric cards */}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <MetricCard title="Edge Requests" metric="Invocations" value={fmtNum(totalReq)} href="/observability"
          data={reqSeries} categories={["Requests", "Errors"]} colors={["blue", "amber"]} label={rangeLabel} />
        <MetricCard title="Cache Hit Ratio" metric="Hits vs Misses" value={`${cacheHitPct.toFixed(1)}%`} href="/observability"
          data={cacheSeries} categories={["Hits", "Misses"]} colors={["blue", "amber"]} label={rangeLabel} />
        <MetricCard title="Functions" metric="Invocations" value={fmtNum(totalReq)} href="/observability"
          data={reqSeries} categories={["Requests"]} colors={["blue"]} label={rangeLabel} />
        <MetricCard title="Middleware Invocations" metric="Invocations" value={fmtNum(totalMw)} href="/firewall"
          data={mwSeries} categories={["Invocations"]} colors={["amber"]} label={rangeLabel} />
      </div>

      {/* Project breakdown */}
      <div className="mt-6">
        <div className="relative mb-3">
          <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
          <input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Search…"
            className="w-full rounded-lg border border-border bg-card py-2.5 pl-9 pr-3 text-sm focus:outline-none focus:ring-2 focus:ring-border"
          />
        </div>
        <Card className="p-0">
          <div className="flex items-center justify-between border-b border-border px-4 py-2.5 text-xs font-medium text-muted">
            <span>Project</span>
            <span className="flex items-center gap-1">Requests <ChevronDown className="h-3 w-3" /></span>
          </div>
          {projects.map((p, i) => (
            <Link
              key={p.project}
              href={`/projects/${encodeURIComponent(p.project)}`}
              className="flex items-center justify-between border-b border-border px-4 py-3 text-sm last:border-0 hover:bg-subtle"
            >
              <span className="flex items-center gap-3">
                <span className="h-4 w-1 rounded-full" style={{ background: BAR_COLORS[i % BAR_COLORS.length] }} />
                <span className="flex h-5 w-5 items-center justify-center rounded-full bg-fg text-bg">
                  <svg width="9" height="8" viewBox="0 0 24 22" aria-hidden><path d="M12 0 L24 22 L0 22 Z" fill="currentColor" /></svg>
                </span>
                <span className="font-medium">{p.project}</span>
              </span>
              <span className="flex items-center gap-3">
                <span className="rounded-md bg-subtle px-2 py-0.5 font-medium tabular-nums">{fmtNum(p.requests)}</span>
                <ChevronRight className="h-4 w-4 text-muted" />
              </span>
            </Link>
          ))}
          {!projects.length && <div className="px-4 py-10 text-center text-sm text-secondary">No traffic in this window yet.</div>}
        </Card>
        <div className="mt-3 flex items-center justify-between text-xs text-muted">
          <span>Show 10</span>
          <span>1 of 1</span>
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
    <Card className="p-5">
      <Link href={href} className="mb-3 flex items-center justify-between">
        <span className="text-sm font-semibold">{title}</span>
        <ChevronRight className="h-4 w-4 text-muted" />
      </Link>
      <div className="mb-1 text-xs text-secondary">{metric}</div>
      <div className="mb-3 text-2xl font-semibold tabular-nums">{value}</div>
      <AreaChart
        className="h-40"
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
      <div className="mt-1 flex justify-between text-[11px] text-muted">
        <span>{label.replace("Last ", "")} ago</span>
        <span>now</span>
      </div>
    </Card>
  );
}
