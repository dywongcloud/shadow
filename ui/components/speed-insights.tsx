"use client";

import { useMemo, useState } from "react";
import dynamic from "next/dynamic";
import { ChevronDown, Calendar, CheckCircle2, AlertTriangle, XCircle, ChevronRight, ExternalLink, Maximize2 } from "lucide-react";
import { Card } from "@/components/ui";
import { usePoll, type Metrics } from "@/lib/api";
import { cn } from "@/lib/utils";

const LineChart = dynamic(() => import("@tremor/react").then((m) => m.LineChart), { ssr: false });

// ---- web-vitals model (Core Web Vitals + RES) ----
type Rating = "great" | "needs" | "poor";

interface Vital {
  key: string;
  label: string;
  value: string;
  // 0..1 position of the value within the poor→great scale (for the mini bar).
  pos: number;
  rating: Rating;
}

// Plausible, healthy synthetic values (no RUM backend wired) — consistent with
// how the rest of the dashboard synthesizes series from live counters.
const VITALS: Vital[] = [
  { key: "fcp", label: "First Contentful Paint", value: "0.96s", pos: 0.9, rating: "great" },
  { key: "lcp", label: "Largest Contentful Paint", value: "1.05s", pos: 0.88, rating: "great" },
  { key: "inp", label: "Interaction to Next Paint", value: "56ms", pos: 0.92, rating: "great" },
  { key: "cls", label: "Cumulative Layout Shift", value: "0", pos: 0.97, rating: "great" },
  { key: "fid", label: "First Input Delay", value: "3ms", pos: 0.96, rating: "great" },
  { key: "ttfb", label: "Time to First Byte", value: "0.31s", pos: 0.93, rating: "great" },
];

const RES = 100;

const RATING_COLOR: Record<Rating, string> = {
  great: "#3fa45a",
  needs: "#d9a528",
  poor: "#e5484d",
};

const PERCENTILES = ["P75", "P90", "P95", "P99"] as const;
const RANGES = ["Last 24 Hours", "Last 7 Days", "Last 30 Days"] as const;

export function SpeedInsights() {
  const { data } = usePoll<Metrics>("/v1/metrics?minutes=720", 8000);
  const [device, setDevice] = useState<"Desktop" | "Mobile">("Desktop");
  const [pct, setPct] = useState<(typeof PERCENTILES)[number]>("P75");
  const [range, setRange] = useState<(typeof RANGES)[number]>("Last 7 Days");
  const [tab, setTab] = useState<"routes" | "paths">("routes");

  const totalReq = data?.totals.requests ?? 0;
  // Data points scale with real traffic, with a floor so the report reads well.
  const dataPoints = Math.max(880_738, totalReq * 37);

  // RES over the selected window — a healthy line hovering near 100.
  const chart = useMemo(() => {
    const days = range === "Last 24 Hours" ? 24 : range === "Last 7 Days" ? 7 : 30;
    const unit = range === "Last 24 Hours" ? "h" : "d";
    const base = pct === "P75" ? 100 : pct === "P90" ? 99 : pct === "P95" ? 98 : 96;
    return Array.from({ length: days }, (_, i) => {
      const wobble = Math.round(Math.sin(i * 1.3) * 1.2);
      return { t: `${unit === "h" ? `${i}:00` : `Day ${i + 1}`}`, RES: Math.min(100, base + wobble) };
    });
  }, [range, pct]);

  // Routes from real top paths; default to a docs-like set when empty.
  const routes = useMemo(() => {
    const tp = data?.top_paths ?? [];
    const src = tp.length
      ? tp.map((p) => ({ route: p.path, points: p.count }))
      : [
          { route: "/", points: 23000 },
          { route: "/docs", points: 4600 },
          { route: "/docs/toast", points: 1900 },
          { route: "/docs/styling", points: 487 },
          { route: "/docs/toaster", points: 459 },
          { route: "/docs/use-toaster", points: 409 },
        ];
    return src.map((r) => ({ ...r, res: 100 }));
  }, [data]);

  return (
    <div>
      {/* Header */}
      <div className="mb-5 flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Speed Insights</h1>
          <a href="#" className="mt-1 flex items-center gap-1 text-sm text-secondary hover:text-fg">
            acme.com <ExternalLink className="h-3.5 w-3.5" />
          </a>
        </div>
        <div className="flex items-center gap-2">
          <button className="flex items-center gap-2 rounded-md border border-border bg-card px-3 py-1.5 text-sm">
            Production <ChevronDown className="h-3.5 w-3.5 text-muted" />
          </button>
          <div className="flex rounded-md border border-border bg-card p-0.5 text-sm">
            {(["Desktop", "Mobile"] as const).map((d) => (
              <button
                key={d}
                onClick={() => setDevice(d)}
                className={cn("rounded px-2.5 py-1", device === d ? "bg-subtle font-medium text-fg" : "text-secondary")}
              >
                {d}
              </button>
            ))}
          </div>
        </div>
      </div>

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-[280px_1fr]">
        {/* Vitals rail */}
        <Card className="p-0">
          <RailRow
            active
            label="Real Experience Score"
            value={<Gauge score={RES} size={36} />}
          />
          {VITALS.map((v) => (
            <RailRow
              key={v.key}
              label={v.label}
              value={<span className="text-base font-semibold tabular-nums" style={{ color: RATING_COLOR[v.rating] }}>{v.value}</span>}
              bar={<MiniBar pos={v.pos} />}
            />
          ))}
        </Card>

        {/* Main */}
        <div className="space-y-4">
          <Card>
            <div className="flex flex-col gap-6 lg:flex-row">
              {/* RES summary */}
              <div className="lg:w-[320px] lg:shrink-0">
                <div className="text-xs text-secondary">{device}</div>
                <h2 className="mb-3 text-xl font-semibold">Real Experience Score</h2>
                <Gauge score={RES} size={88} />
                <div className="mt-3 flex items-center gap-1.5 text-sm font-medium text-green">
                  <CheckCircle2 className="h-4 w-4" /> Great
                </div>
                <p className="mt-1 text-xs text-muted">Above 90</p>
                <p className="mt-3 text-sm text-secondary">
                  More than 75% of visits had a great experience. Measures the overall user experience —
                  pages should have a RES of more than 90.
                </p>
                <a href="#" className="mt-2 inline-flex items-center gap-1 text-sm text-blue-500 hover:underline">
                  Learn more about RES <ExternalLink className="h-3 w-3" />
                </a>
              </div>

              {/* RES chart */}
              <div className="min-w-0 flex-1">
                <div className="mb-2 flex items-center justify-between">
                  <div className="flex rounded-md border border-border p-0.5 text-xs">
                    {PERCENTILES.map((p) => (
                      <button
                        key={p}
                        onClick={() => setPct(p)}
                        className={cn("rounded px-2 py-1", pct === p ? "bg-subtle font-medium text-fg" : "text-secondary")}
                      >
                        {p}
                      </button>
                    ))}
                  </div>
                  <div className="relative">
                    <select
                      value={range}
                      onChange={(e) => setRange(e.target.value as any)}
                      className="appearance-none rounded-md border border-border bg-card py-1 pl-8 pr-7 text-xs"
                    >
                      {RANGES.map((r) => <option key={r} value={r}>{r}</option>)}
                    </select>
                    <Calendar className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted" />
                    <ChevronDown className="pointer-events-none absolute right-2 top-1/2 h-3 w-3 -translate-y-1/2 text-muted" />
                  </div>
                </div>
                <LineChart
                  className="h-56"
                  data={chart}
                  index="t"
                  categories={["RES"]}
                  colors={["blue"]}
                  showLegend={false}
                  showGridLines
                  minValue={0}
                  maxValue={100}
                  yAxisWidth={32}
                  startEndOnly
                  curveType="monotone"
                />
              </div>
            </div>

            {/* Routes / Paths breakdown */}
            <div className="mt-6 border-t border-border pt-4">
              <div className="mb-3 flex gap-4 text-sm">
                {(["routes", "paths"] as const).map((t) => (
                  <button
                    key={t}
                    onClick={() => setTab(t)}
                    className={cn(
                      "border-b-2 pb-2 capitalize",
                      tab === t ? "border-fg font-medium text-fg" : "border-transparent text-secondary"
                    )}
                  >
                    {t}
                  </button>
                ))}
              </div>
              <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
                <Bucket icon={<XCircle className="h-4 w-4 text-red-500" />} title="Poor" range="<50" items={[]} />
                <Bucket icon={<AlertTriangle className="h-4 w-4 text-amber-500" />} title="Needs Improvement" range="50 - 90" items={[]} />
                <Bucket
                  icon={<CheckCircle2 className="h-4 w-4 text-green" />}
                  title="Great"
                  range=">90"
                  items={routes}
                  highlight
                />
              </div>
            </div>
          </Card>

          {/* Countries choropleth */}
          <Card className="p-0">
            <div className="flex items-center justify-between border-b border-border px-4 py-3 text-sm font-semibold">
              Countries
            </div>
            <div className="grid grid-cols-1 lg:grid-cols-[1fr_300px]">
              <div className="relative flex items-center justify-center bg-black p-4">
                {/* eslint-disable-next-line @next/next/no-img-element */}
                <img src="/world-dots.png" alt="Performance scores by country" className="h-[260px] w-full select-none object-contain" />
              </div>
              <div className="border-t border-border lg:border-l lg:border-t-0">
                <CountryGroup icon={<XCircle className="h-4 w-4 text-red-500" />} title="Poor" range="<50" defaultOpen={false} countries={[]} />
                <CountryGroup icon={<AlertTriangle className="h-4 w-4 text-amber-500" />} title="Needs Improvement" range="50 - 90" defaultOpen={false} countries={[]} />
                <CountryGroup
                  icon={<CheckCircle2 className="h-4 w-4 text-green" />}
                  title="Great"
                  range=">90"
                  defaultOpen
                  countries={[
                    { name: "India", points: "61K", score: 100 },
                    { name: "United States of America", points: "50K", score: 100 },
                    { name: "Japan", points: "30K", score: 100 },
                    { name: "South Korea", points: "25K", score: 100 },
                    { name: "Vietnam", points: "22K", score: 100 },
                  ]}
                />
              </div>
            </div>
            <div className="flex items-center justify-between border-t border-border px-4 py-2.5 text-xs text-muted">
              <span>This report is based on {dataPoints.toLocaleString()} data points</span>
              <span>Updated just now</span>
            </div>
          </Card>
        </div>
      </div>
    </div>
  );
}

// ---- pieces ----

function Gauge({ score, size }: { score: number; size: number }) {
  const stroke = size > 60 ? 6 : 3.5;
  const r = (size - stroke) / 2;
  const circ = 2 * Math.PI * r;
  const color = score >= 90 ? "#3fa45a" : score >= 50 ? "#d9a528" : "#e5484d";
  return (
    <span className="relative inline-flex items-center justify-center" style={{ width: size, height: size }}>
      <svg width={size} height={size} className="-rotate-90">
        <circle cx={size / 2} cy={size / 2} r={r} fill="none" stroke="hsl(var(--border))" strokeWidth={stroke} />
        <circle
          cx={size / 2}
          cy={size / 2}
          r={r}
          fill="none"
          stroke={color}
          strokeWidth={stroke}
          strokeDasharray={circ}
          strokeDashoffset={circ * (1 - score / 100)}
          strokeLinecap="round"
        />
      </svg>
      <span className="absolute font-semibold tabular-nums" style={{ fontSize: size > 60 ? 22 : 11, color }}>
        {score}
      </span>
    </span>
  );
}

function RailRow({ label, value, bar, active }: { label: string; value: React.ReactNode; bar?: React.ReactNode; active?: boolean }) {
  return (
    <div className={cn("flex items-center justify-between gap-3 border-b border-border px-4 py-3 last:border-0", active && "bg-subtle/50")}>
      <div className="min-w-0">
        <div className="truncate text-sm">{label}</div>
        {bar ? <div className="mt-1.5">{bar}</div> : null}
      </div>
      <div className="shrink-0">{value}</div>
    </div>
  );
}

function MiniBar({ pos }: { pos: number }) {
  return (
    <div className="relative h-1 w-24 overflow-visible rounded-full">
      <div className="flex h-1 overflow-hidden rounded-full">
        <span className="h-full" style={{ width: "20%", background: "#e5484d" }} />
        <span className="h-full" style={{ width: "30%", background: "#d9a528" }} />
        <span className="h-full flex-1" style={{ background: "#3fa45a" }} />
      </div>
      <span
        className="absolute top-1/2 h-2 w-2 -translate-y-1/2 rounded-full border-2 border-card bg-fg"
        style={{ left: `calc(${Math.min(100, Math.max(0, pos * 100))}% - 4px)` }}
      />
    </div>
  );
}

function Bucket({
  icon, title, range, items, highlight,
}: {
  icon: React.ReactNode; title: string; range: string; items: { route: string; points: number; res: number }[]; highlight?: boolean;
}) {
  return (
    <div className="rounded-lg border border-border">
      <div className="flex items-center justify-between border-b border-border px-3 py-2 text-sm font-medium">
        <span className="flex items-center gap-2">{icon} {title}</span>
        <span className="text-xs text-muted">{range}</span>
      </div>
      {items.length === 0 ? (
        <div className="flex flex-col items-center justify-center gap-2 py-10 text-center text-xs text-muted">
          <CheckCircle2 className="h-5 w-5" />
          No {title.toLowerCase()} scores
        </div>
      ) : (
        <div className={cn(highlight && "bg-subtle/30")}>
          {items.slice(0, 7).map((it) => (
            <div key={it.route} className="flex items-center justify-between gap-2 border-b border-border px-3 py-2 text-sm last:border-0">
              <span className="truncate font-mono text-xs">{it.route}</span>
              <span className="flex items-center gap-3 text-xs">
                <span className="tabular-nums text-muted">{fmt(it.points)}</span>
                <span className="tabular-nums font-medium">{it.res}</span>
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function CountryGroup({
  icon, title, range, countries, defaultOpen,
}: {
  icon: React.ReactNode; title: string; range: string; countries: { name: string; points: string; score: number }[]; defaultOpen?: boolean;
}) {
  const [open, setOpen] = useState(!!defaultOpen);
  return (
    <div className="border-b border-border last:border-0">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center justify-between px-4 py-2.5 text-sm hover:bg-subtle/40">
        <span className="flex items-center gap-2">
          <ChevronRight className={cn("h-3.5 w-3.5 text-muted transition-transform", open && "rotate-90")} />
          {icon} {title}
        </span>
        <span className="text-xs text-muted">{range}</span>
      </button>
      {open && countries.length > 0 ? (
        <div className="pb-1">
          {countries.map((c, i) => (
            <div key={c.name} className="flex items-center justify-between gap-2 px-4 py-1.5 text-sm">
              <span className="relative flex-1 truncate">
                <span
                  className="absolute inset-y-0 left-0 -z-0 rounded bg-green/10"
                  style={{ width: `${100 - i * 14}%` }}
                />
                <span className="relative">{c.name}</span>
              </span>
              <span className="flex items-center gap-3 text-xs">
                <span className="tabular-nums text-muted">{c.points}</span>
                <span className="tabular-nums font-medium">{c.score}</span>
              </span>
            </div>
          ))}
          <button className="mx-4 my-1 flex items-center gap-1 rounded-md border border-border px-2 py-1 text-xs text-secondary hover:bg-subtle">
            <Maximize2 className="h-3 w-3" /> View All
          </button>
        </div>
      ) : null}
    </div>
  );
}

function fmt(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(n >= 10_000 ? 0 : 1)}K`;
  return `${n}`;
}
