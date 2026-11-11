"use client";

import { use, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { Badge, Button, Card } from "@/components/ui";
import { usePoll, type MetricBucket, type Metrics } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { AnalyticsMetricCard, type MetricDeltaTone } from "@/components/analytics-metric-card";
import { AnalyticsChart, type ChartColumn, type ChartSeries } from "@/components/analytics-chart";
import { AnalyticsBreakdown, type BreakdownRow, type BreakdownTab } from "@/components/analytics-breakdown";

const WINDOWS = {
  "60m": { label: "60m", minutes: 60, gran: "minute", bucketMs: 60_000 },
  "24h": { label: "24h", minutes: 1_440, gran: "hour", bucketMs: 3_600_000 },
  "7d": { label: "7d", minutes: 10_080, gran: "day", bucketMs: 86_400_000 },
} as const;

type WindowKey = keyof typeof WINDOWS;
const WINDOW_KEYS: WindowKey[] = ["60m", "24h", "7d"];

const PALETTE_CSS = [
  ".hive-analytics {",
  "  --a-indigo: #4239c2; --a-rose: #9d1239; --a-amber: #826001;",
  "  --a-emerald: #0c8d63; --a-cyan: #1296aa; --a-violet: #896cd8;",
  "  --a-indigo-lit: #6366f1; --a-cyan-lit: #22d3ee; --a-emerald-lit: #34d399;",
  "  --a-amber-lit: #fbbf24; --a-rose-lit: #fb7185; --a-violet-lit: #a78bfa;",
  "}",
  ".dark .hive-analytics {",
  "  --a-indigo: #4d4ad3; --a-rose: #ae2846; --a-amber: #8c6800;",
  "  --a-emerald: #07976a; --a-cyan: #149db2; --a-violet: #8163ce;",
  "}",
  "@media (forced-colors: active) {",
  "  .hive-analytics .ha-fill { background-image: repeating-linear-gradient(45deg, CanvasText 0 1px, transparent 1px 6px) !important; }",
  "}",
].join(String.fromCharCode(10));

const ROLES = {
  indigo: { color: "var(--a-indigo)", lit: "var(--a-indigo-lit)" },
  cyan: { color: "var(--a-cyan)", lit: "var(--a-cyan-lit)" },
  emerald: { color: "var(--a-emerald)", lit: "var(--a-emerald-lit)" },
  amber: { color: "var(--a-amber)", lit: "var(--a-amber-lit)" },
  rose: { color: "var(--a-rose)", lit: "var(--a-rose-lit)" },
  violet: { color: "var(--a-violet)", lit: "var(--a-violet-lit)" },
};
const BAR_ROLES = [ROLES.indigo, ROLES.cyan, ROLES.emerald, ROLES.amber, ROLES.rose, ROLES.violet];
const EMPTY_BUCKETS: MetricBucket[] = [];
const EMPTY_PATHS: { path: string; count: number }[] = [];
const EMPTY_STATUS: Record<string, number> = {};
const COUNT = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });

function finite(value: number | null | undefined): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function fmtCount(value: number): string {
  return COUNT.format(finite(value));
}

function fmtPct(value: number): string {
  return (finite(value) * 100).toFixed(1) + "%";
}

const SERIES_META: ChartSeries[] = [
  { key: "requests", label: "Requests", color: ROLES.indigo.color, lit: ROLES.indigo.lit, format: fmtCount },
  { key: "errors", label: "Errors", color: ROLES.rose.color, lit: ROLES.rose.lit, format: fmtCount },
  { key: "blocked", label: "Blocked", color: ROLES.amber.color, lit: ROLES.amber.lit, format: fmtCount },
  { key: "cache", label: "Cache hit ratio", color: ROLES.emerald.color, lit: ROLES.emerald.lit, format: fmtPct },
];

function formatBucketLabel(t_ms: number, key: WindowKey): string {
  const date = new Date(t_ms);
  if (key === "7d") return date.toLocaleDateString("en-US", { month: "short", day: "numeric" });
  if (key === "24h") return date.toLocaleTimeString("en-US", { hour: "2-digit" });
  return date.toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit" });
}

function metricValue(bucket: MetricBucket, key: string): number {
  if (key === "requests") return finite(bucket.requests);
  if (key === "errors") return finite(bucket.errors) + finite(bucket.client_err);
  if (key === "blocked") return finite(bucket.blocked);
  const hits = finite(bucket.cache_hits);
  const misses = finite(bucket.cache_miss);
  return hits + misses > 0 ? hits / (hits + misses) : 0;
}

function valuesFor(bucket: MetricBucket): Record<string, number> {
  const values: Record<string, number> = {};
  for (const item of SERIES_META) values[item.key] = metricValue(bucket, item.key);
  return values;
}

function halfComparison(values: number[], format: (value: number) => string, positiveIsGood: boolean): {
  delta: string;
  tone: MetricDeltaTone;
  hint: string;
  aria: string;
} {
  if (values.length < 2) {
    const hint = "Fewer than two measured buckets.";
    return { delta: "No comparison", tone: "default", hint, aria: "No comparison: " + hint };
  }
  const middle = Math.floor(values.length / 2);
  const first = values.slice(0, middle);
  const second = values.slice(middle);
  let firstSum = 0;
  let secondSum = 0;
  for (const value of first) firstSum += finite(value);
  for (const value of second) secondSum += finite(value);
  const firstAvg = firstSum / first.length;
  const secondAvg = secondSum / second.length;
  let pct: number | null = firstAvg === 0 ? (secondAvg === 0 ? 0 : null) : ((secondAvg - firstAvg) / firstAvg) * 100;
  if (pct !== null && !Number.isFinite(pct)) pct = null;
  const delta = pct === null ? "No baseline" : (pct >= 0 ? "+" : "−") + Math.abs(pct).toFixed(1) + "%";
  const tone: MetricDeltaTone = pct === null || pct === 0 ? "default" : (pct > 0) === positiveIsGood ? "green" : "red";
  const hint = "First half avg " + format(firstAvg) + " · second half avg " + format(secondAvg);
  return { delta, tone, hint, aria: "Change " + delta + "; " + hint };
}

function stableRole(label: string): { color: string; lit: string } {
  let hash = 0;
  for (let index = 0; index < label.length; index += 1) hash = (hash * 31 + label.charCodeAt(index)) >>> 0;
  return BAR_ROLES[hash % BAR_ROLES.length] ?? ROLES.indigo;
}

function statusRole(key: string): { color: string; lit: string } {
  const first = key.trim().charAt(0);
  if (first === "1") return ROLES.violet;
  if (first === "2") return ROLES.emerald;
  if (first === "3") return ROLES.cyan;
  if (first === "4") return ROLES.amber;
  if (first === "5") return ROLES.rose;
  return ROLES.indigo;
}

export function AnalyticsPageClient({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const name = decodeURIComponent(params.project);
  const [rangeKey, setRangeKey] = useState<WindowKey>("60m");
  const [activeKeys, setActiveKeys] = useState<string[]>(SERIES_META.map((item) => item.key));
  const win = WINDOWS[rangeKey];
  const metricsPath = "/v1/metrics?project=" + encodeURIComponent(name) + "&minutes=" + win.minutes + "&gran=" + win.gran;
  const { data, error, loading, refresh } = usePoll<Metrics>(metricsPath, 8000);
  const series = data?.series ?? EMPTY_BUCKETS;
  const [nowMs, setNowMs] = useState<number | null>(null);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setNowMs(Date.now());
  }, [data, metricsPath]);

  function toggleSeries(key: string) {
    setActiveKeys((previous) => {
      if (previous.includes(key)) return previous.length === 1 ? previous : previous.filter((item) => item !== key);
      return SERIES_META.filter((item) => previous.includes(item.key) || item.key === key).map((item) => item.key);
    });
  }

  const chartData = useMemo(() => {
    const columns: ChartColumn[] = series.map((bucket) => ({
      t_ms: bucket.t_ms,
      label: formatBucketLabel(bucket.t_ms, rangeKey),
      values: valuesFor(bucket),
    }));
    if (series.length < 2) return { columns, solidEndIndex: columns.length - 1, projectionIndex: null };
    const last = series[series.length - 1];
    if (nowMs === null) return { columns, solidEndIndex: columns.length - 1, projectionIndex: null };
    const elapsed = nowMs - last.t_ms;
    const fraction = Math.max(0, Math.min(1, elapsed / win.bucketMs));
    if (fraction <= 0 || fraction >= 1) return { columns, solidEndIndex: columns.length - 1, projectionIndex: null };
    const measuredValues = valuesFor(last);
    const projectedValues: Record<string, number> = {};
    for (const item of SERIES_META) projectedValues[item.key] = finite(measuredValues[item.key]) / fraction;
    const projected: ChartColumn = {
      t_ms: last.t_ms + win.bucketMs,
      label: "Projected full bucket",
      values: projectedValues,
      measuredValues,
      projected: true,
    };
    return { columns: columns.concat([projected]), solidEndIndex: columns.length - 2, projectionIndex: columns.length };
  }, [series, rangeKey, win.bucketMs, nowMs]);

  const metricCards = useMemo(() => {
    if (!data) return [];
    const totals = data.totals;
    return SERIES_META.map((item) => {
      const values = series.map((bucket) => metricValue(bucket, item.key));
      let total = 0;
      if (item.key === "requests") total = finite(totals.requests);
      else if (item.key === "errors") total = finite(totals.errors);
      else if (item.key === "blocked") total = finite(totals.blocked);
      else total = finite(totals.cache_hit_ratio);
      const comparison = halfComparison(values, item.format, item.key === "requests" || item.key === "cache");
      return {
        key: item.key,
        label: item.label,
        value: item.format(total),
        values,
        delta: comparison.delta,
        deltaTone: comparison.tone,
        deltaHint: comparison.hint,
        deltaAria: comparison.aria,
        color: item.color,
        lit: item.lit,
      };
    });
  }, [data, series]);

  const statusRows = useMemo(() => {
    const source = data?.status_distribution ?? EMPTY_STATUS;
    const rows = Object.keys(source).map((key) => {
      const role = statusRole(key);
      return { id: key, label: key, count: finite(source[key]), color: role.color, lit: role.lit };
    });
    rows.sort((a, b) => a.label.localeCompare(b.label));
    return rows;
  }, [data]);

  const topPathRows = useMemo<BreakdownRow[]>(() => {
    const source = data?.top_paths ?? EMPTY_PATHS;
    return source.map((row, index) => {
      const role = stableRole(row.path);
      return { id: index + ":" + row.path, label: row.path, count: finite(row.count), color: role.color, lit: role.lit };
    });
  }, [data]);

  let statusMax = 0;
  for (const row of statusRows) if (finite(row.count) > statusMax) statusMax = finite(row.count);
  const lastBucket = series.length ? series[series.length - 1] : null;
  const updated = lastBucket ? "Last bucket " + timeAgo(lastBucket.t_ms) : "Waiting for the metrics response";

  const leftTabs: BreakdownTab[] = [
    {
      id: "pages",
      label: "Pages",
      rows: topPathRows,
      empty: "No top paths are returned.",
      caption: "Top paths returned for this tenant; the metrics endpoint does not scope them to one project.",
    },
    { id: "routes", label: "Routes", rows: [], empty: "The metrics endpoint does not return route-level counts." },
    { id: "hostnames", label: "Hostnames", rows: [], empty: "The metrics endpoint does not return hostnames." },
  ];
  const rightTabs: BreakdownTab[] = [
    { id: "referrers", label: "Referrers", rows: [], empty: "The metrics endpoint does not return referrers." },
    { id: "utm", label: "UTM Parameters", rows: [], empty: "The metrics endpoint does not return UTM parameters." },
  ];

  return (
    <div className="hive-analytics min-w-0 w-full max-w-full px-4 py-5 sm:px-6">
      <style>{PALETTE_CSS}</style>
      <header className="mb-4 flex min-w-0 flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0">
          <div className="min-w-0 break-words text-xs font-medium uppercase tracking-wide text-muted">Project analytics</div>
          <h1 className="mt-1 min-w-0 break-words text-xl font-semibold text-fg sm:text-2xl">{name}</h1>
          <p className="mt-1 min-w-0 break-words text-sm text-secondary">Measured request series, status classes and top paths. The dashed trailing segment is a projection from the current partial bucket, not a measurement.</p>
        </div>
        <Link
          href={"/projects/" + encodeURIComponent(name)}
          className="inline-flex min-h-[44px] min-w-0 max-w-full items-center rounded-md border border-border px-3 text-sm font-medium text-fg hover:bg-subtle"
          style={{ borderColor: "var(--a-indigo)" }}
        >
          <span className="min-w-0 break-words">Project overview</span>
        </Link>
      </header>
      <div role="group" aria-label="Window" className="mb-4 flex min-w-0 flex-wrap items-center gap-2">
        <span className="shrink-0 text-xs font-medium uppercase tracking-wide text-muted">Window</span>
        {WINDOW_KEYS.map((key) => {
          const isActive = key === rangeKey;
          return (
            <Button
              key={key}
              variant={isActive ? "default" : "outline"}
              aria-pressed={isActive}
              className="min-h-[44px] min-w-0 px-3"
              style={isActive ? { background: "linear-gradient(90deg, var(--a-indigo), var(--a-cyan))", color: "#ffffff", borderColor: "transparent" } : undefined}
              onClick={() => setRangeKey(key)}
            >
              {WINDOWS[key].label}
            </Button>
          );
        })}
        <Button
          variant="outline"
          aria-label="Refresh measured metrics"
          className="min-h-[44px] min-w-0"
          style={{ borderColor: "var(--a-violet)" }}
          onClick={() => refresh()}
        >
          Refresh
        </Button>
        <Badge tone="purple" className="min-w-0 max-w-full">
          <span className="min-w-0 break-words">{updated}</span>
        </Badge>
      </div>
      {error ? (
        <div role="alert" className="mb-4 flex min-w-0 flex-wrap items-center gap-2 rounded-md border border-red-500/40 bg-red-500/5 p-3 text-sm text-red-500">
          <Badge tone="red" className="shrink-0">Last response failed</Badge>
          <span className="min-w-0 break-words">{error}</span>
        </div>
      ) : null}
      {!data && loading ? (
        <Card className="min-w-0 w-full max-w-full p-5">
          <p className="min-w-0 break-words text-sm text-secondary">Waiting for the metrics response.</p>
        </Card>
      ) : !data ? (
        <Card className="min-w-0 w-full max-w-full p-5">
          <p className="min-w-0 break-words text-sm text-secondary">The metrics request has not returned data.</p>
        </Card>
      ) : series.length === 0 ? (
        <Card className="relative min-w-0 w-full max-w-full overflow-hidden p-5">
          <span className="absolute inset-y-0 left-0 w-1.5" style={{ background: "linear-gradient(180deg, var(--a-indigo-lit), var(--a-emerald))" }} aria-hidden="true" />
          <p className="min-w-0 break-words text-sm text-secondary">No measured buckets are returned for this window.</p>
        </Card>
      ) : (
        <div aria-busy={loading} className={loading ? "min-w-0 w-full max-w-full opacity-80" : "min-w-0 w-full max-w-full"}>
          <div className="grid min-w-0 grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
            {metricCards.map((card) => (
              <AnalyticsMetricCard
                key={card.key}
                label={card.label}
                value={card.value}
                values={card.values}
                delta={card.delta}
                deltaTone={card.deltaTone}
                deltaHint={card.deltaHint}
                deltaAria={card.deltaAria}
                color={card.color}
                lit={card.lit}
              />
            ))}
          </div>
          <div className="mt-4 min-w-0 w-full max-w-full">
            <AnalyticsChart
              columns={chartData.columns}
              series={SERIES_META}
              activeKeys={activeKeys}
              onToggle={toggleSeries}
              solidEndIndex={chartData.solidEndIndex}
              projectionIndex={chartData.projectionIndex}
              windowLabel={win.label}
            />
          </div>
          <Card className="mt-4 min-w-0 w-full max-w-full p-4 sm:p-5">
            <div className="mb-2 flex min-w-0 items-center justify-between gap-2">
              <h2 className="min-w-0 break-words text-sm font-semibold text-fg">Status</h2>
              <Badge tone="blue" className="shrink-0 tabular-nums">{statusRows.length + " rows"}</Badge>
            </div>
            <p className="mb-3 min-w-0 break-words text-xs text-secondary">Status classes returned for this tenant.</p>
            {statusRows.length === 0 ? (
              <div className="flex min-h-[44px] min-w-0 items-center gap-3 rounded-md border border-dashed border-border p-3">
                <span className="h-8 w-1.5 shrink-0 rounded-full" style={{ background: "linear-gradient(180deg, var(--a-rose-lit), var(--a-violet))" }} aria-hidden="true" />
                <span className="min-w-0 break-words text-sm text-secondary">No status classes are returned.</span>
              </div>
            ) : (
              <div className="grid min-w-0 grid-cols-1 gap-2 sm:grid-cols-2 xl:grid-cols-3">
                {statusRows.map((row) => {
                  const count = finite(row.count);
                  const width = statusMax > 0 ? (count / statusMax) * 100 : 0;
                  return (
                    <div key={row.id} className="relative min-w-0 overflow-hidden rounded-md border border-border p-3">
                      <span
                        className="ha-fill pointer-events-none absolute inset-y-1 left-0 rounded-r-full"
                        style={{ width: width + "%", background: "linear-gradient(90deg, " + row.lit + ", " + row.color + ")", opacity: 0.24 }}
                        role="meter"
                        aria-label={row.label + " Requests"}
                        aria-valuemin={0}
                        aria-valuemax={statusMax}
                        aria-valuenow={count}
                        aria-valuetext={fmtCount(count) + " Requests"}
                      />
                      <div className="relative z-10 flex min-w-0 items-center justify-between gap-2">
                        <span className="min-w-0 break-words text-sm text-fg">{row.label}</span>
                        <span className="shrink-0 text-sm font-medium tabular-nums text-fg">{fmtCount(count)}</span>
                      </div>
                    </div>
                  );
                })}
              </div>
            )}
          </Card>
          <div className="mt-4 grid min-w-0 grid-cols-1 gap-4 lg:grid-cols-2">
            <AnalyticsBreakdown title="Pages" tabs={leftTabs} labelHeading="Page" countLabel="Requests" />
            <AnalyticsBreakdown title="Referrers and UTM parameters" tabs={rightTabs} labelHeading="Dimension" countLabel="Requests" />
          </div>
          <p className="mt-4 min-w-0 break-words text-xs text-secondary">
            Project-filtered buckets carry request counts; the backend does not return per-project error, blocked, or cache counters in this endpoint.
          </p>
        </div>
      )}
    </div>
  );
}
