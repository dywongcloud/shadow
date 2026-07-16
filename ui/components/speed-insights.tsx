"use client";

import { useMemo, useState } from "react";
import dynamic from "next/dynamic";
import { ChevronDown, Calendar, CheckCircle2, AlertTriangle, XCircle, ChevronRight, ExternalLink, Maximize2 } from "lucide-react";
import { Card } from "@/components/ui";
import { usePoll } from "@/lib/api";
import { cn } from "@/lib/utils";
import Image from "next/image";

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

interface VitalPercentiles {
  fcp: number | null;
  lcp: number | null;
  cls: number | null;
  inp: number | null;
  ttfb: number | null;
}
interface RouteScore {
  route: string;
  count: number;
  res: number | null;
  p75: VitalPercentiles;
}
interface RumSummary {
  sample_count: number;
  res: number | null;
  p75: VitalPercentiles;
  p90: VitalPercentiles;
  p95: VitalPercentiles;
  p99: VitalPercentiles;
  routes: RouteScore[];
}

// Real User Monitoring, backed by GET /v1/speed-insights (the
// @vercel/speed-insights beacon's FCP/LCP/CLS/INP/TTFB samples, stored +
// percentiled server-side — see fluid-gateway's RumStore). Good/poor
// thresholds mirror the server's own res_score() exactly, so the rail's
// great/needs/poor classification and the RES gauge always agree.
const VITAL_LABELS: { key: keyof VitalPercentiles; label: string; good: number; poor: number }[] = [
  { key: "fcp", label: "First Contentful Paint", good: 1800, poor: 3000 },
  { key: "lcp", label: "Largest Contentful Paint", good: 2500, poor: 4000 },
  { key: "inp", label: "Interaction to Next Paint", good: 200, poor: 500 },
  { key: "cls", label: "Cumulative Layout Shift", good: 0.1, poor: 0.25 },
  { key: "ttfb", label: "Time to First Byte", good: 800, poor: 1800 },
];

function ratingOf(value: number, good: number, poor: number): Rating {
  if (value <= good) return "great";
  if (value <= poor) return "needs";
  return "poor";
}

function fmtVital(key: keyof VitalPercentiles, value: number): string {
  if (key === "cls") return value.toFixed(2);
  return value >= 1000 ? `${(value / 1000).toFixed(2)} s` : `${Math.round(value)} ms`;
}

const RATING_COLOR: Record<Rating, string> = {
  great: "#3fa45a",
  needs: "#d9a528",
  poor: "#e5484d",
};

const PERCENTILES = ["P75", "P90", "P95", "P99"] as const;
const RANGES = ["Last 24 Hours", "Last 7 Days", "Last 30 Days"] as const;
const RANGE_MINUTES: Record<(typeof RANGES)[number], number> = {
  "Last 24 Hours": 1_440,
  "Last 7 Days": 10_080,
  "Last 30 Days": 43_200,
};

export function SpeedInsights() {
  const [device, setDevice] = useState<"Desktop" | "Mobile">("Desktop");
  const [pct, setPct] = useState<(typeof PERCENTILES)[number]>("P75");
  const [range, setRange] = useState<(typeof RANGES)[number]>("Last 7 Days");
  const [tab, setTab] = useState<"routes" | "paths">("routes");
  const { data } = usePoll<RumSummary>(
    `/v1/speed-insights?minutes=${RANGE_MINUTES[range]}&device=${device.toLowerCase()}`,
    15_000,
  );

  const res = data?.res ?? null;
  const rumDataPoints = data?.sample_count ?? 0;
  const hasRum = rumDataPoints > 0;
  // No time-bucketed RUM history yet (only an aggregate over the selected
  // range) — show the real current score as a single point rather than
  // fabricating a trend line the backend doesn't actually have data for.
  const chart: { t: string; RES: number }[] = res !== null ? [{ t: "now", RES: res }] : [];

  const selectedPercentiles: VitalPercentiles | undefined = data
    ? { P75: data.p75, P90: data.p90, P95: data.p95, P99: data.p99 }[pct]
    : undefined;

  const VITALS: Vital[] = useMemo(() => {
    if (!selectedPercentiles) return [];
    return VITAL_LABELS.flatMap(({ key, label, good, poor }) => {
      const value = selectedPercentiles[key];
      if (value === null || value === undefined) return [];
      const rating = ratingOf(value, good, poor);
      const pos = 1 - Math.min(1, Math.max(0, (value - good) / (poor - good)));
      return [{ key, label, value: fmtVital(key, value), pos, rating }];
    });
  }, [selectedPercentiles]);

  // Real per-route scores (each route's own p75 + RES, computed server-side
  // the same way as the aggregate) — a route only ever lands in the bucket
  // its OWN performance earns, never forced into "Great" just because it's
  // the only bucket with data.
  const routeBuckets = useMemo(() => {
    const buckets: Record<Rating, { route: string; points: number; res: number | null }[]> = { great: [], needs: [], poor: [] };
    for (const r of data?.routes ?? []) {
      const rating: Rating = r.res === null ? "great" : r.res >= 90 ? "great" : r.res >= 50 ? "needs" : "poor";
      buckets[rating].push({ route: r.route, points: r.count, res: r.res });
    }
    return buckets;
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
            value={<Gauge score={res} size={36} />}
          />
          {(VITALS.length ? VITALS : VITAL_LABELS.map((l) => ({ key: l.key, label: l.label, value: "—", pos: 0, rating: "great" as Rating }))).map((v) => (
            <RailRow
              key={v.key}
              label={v.label}
              value={<span className={cn("text-base font-semibold tabular-nums", VITALS.length ? "" : "text-muted")}>{v.value}</span>}
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
                <Gauge score={res} size={88} />
                {hasRum ? (
                  <p className="mt-3 text-sm text-secondary">
                    Based on {rumDataPoints.toLocaleString()} real-user samples over {range.toLowerCase()}.
                  </p>
                ) : (
                  <>
                    <div className="mt-3 flex items-center gap-1.5 text-sm font-medium text-muted">
                      <AlertTriangle className="h-4 w-4" /> Collecting
                    </div>
                    <p className="mt-3 text-sm text-secondary">
                      Core Web Vitals are measured from real visits via the RUM beacon. No real-user
                      samples have been collected yet, so no score is shown — values populate here once
                      the beacon reports FCP/LCP/INP/CLS/TTFB.
                    </p>
                  </>
                )}
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
                <Bucket icon={<XCircle className="h-4 w-4 text-red-500" />} title="Poor" range="<50" items={routeBuckets.poor} />
                <Bucket icon={<AlertTriangle className="h-4 w-4 text-amber-500" />} title="Needs Improvement" range="50 - 90" items={routeBuckets.needs} />
                <Bucket
                  icon={<CheckCircle2 className="h-4 w-4 text-green" />}
                  title="Great"
                  range=">90"
                  items={routeBuckets.great}
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
                <Image src="/world-dots.png" alt="Performance scores by country" width={3840} height={2400} sizes="(max-width: 1024px) 100vw, 720px" className="h-[260px] w-full select-none object-contain" />
              </div>
              <div className="border-t border-border lg:border-l lg:border-t-0">
                <CountryGroup icon={<XCircle className="h-4 w-4 text-red-500" />} title="Poor" range="<50" defaultOpen={false} countries={[]} />
                <CountryGroup icon={<AlertTriangle className="h-4 w-4 text-amber-500" />} title="Needs Improvement" range="50 - 90" defaultOpen={false} countries={[]} />
                <CountryGroup
                  icon={<CheckCircle2 className="h-4 w-4 text-green" />}
                  title="Great"
                  range=">90"
                  defaultOpen
                  countries={[]}
                />
              </div>
            </div>
            <div className="flex items-center justify-between border-t border-border px-4 py-2.5 text-xs text-muted">
              <span>Based on {rumDataPoints.toLocaleString()} real-user data points{hasRum ? "" : " — RUM beacon not yet reporting"}</span>
              <span>{hasRum ? "Updated just now" : "—"}</span>
            </div>
          </Card>
        </div>
      </div>
    </div>
  );
}

// ---- pieces ----

function Gauge({ score, size }: { score: number | null; size: number }) {
  const stroke = size > 60 ? 6 : 3.5;
  const r = (size - stroke) / 2;
  const circ = 2 * Math.PI * r;
  // No RUM score yet → an empty ring + em-dash, never a fabricated number.
  const color = score === null ? "hsl(var(--muted))" : score >= 90 ? "#3fa45a" : score >= 50 ? "#d9a528" : "#e5484d";
  return (
    <span className="relative inline-flex items-center justify-center" style={{ width: size, height: size }}>
      <svg width={size} height={size} className="-rotate-90">
        <circle cx={size / 2} cy={size / 2} r={r} fill="none" stroke="hsl(var(--border))" strokeWidth={stroke} />
        {score !== null && (
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
        )}
      </svg>
      <span className="absolute font-semibold tabular-nums" style={{ fontSize: size > 60 ? 22 : 11, color }}>
        {score === null ? "—" : score}
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
  icon: React.ReactNode; title: string; range: string; items: { route: string; points: number; res: number | null }[]; highlight?: boolean;
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
                <span className="tabular-nums font-medium">{it.res ?? "—"}</span>
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
