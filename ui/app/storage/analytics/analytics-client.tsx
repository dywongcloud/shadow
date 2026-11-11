"use client";

import { useCallback, useEffect, useId, useMemo, useState } from "react";
import Link from "next/link";
import { ArrowLeft, RefreshCw } from "lucide-react";
import { Badge, Button, Card, PageHeader, StateNotice } from "@/components/ui";
import { apiGet, usePoll, type Database, type DbInstanceMetrics } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import {
  StorageAnalyticsChart,
  chartSegments,
  segmentPath,
  type StorageChartColumn,
  type StorageChartSeries,
} from "@/components/storage-analytics-chart";
import {
  StorageAnalyticsBreakdown,
  StorageAnalyticsEvents,
  type StorageBreakdownRow,
  type StorageEventRow,
} from "@/components/storage-analytics-breakdown";

/** Every new wire key is OPTIONAL: a node rolled before the series payload answers with counters
 *  only. An absent reading never becomes a zero. */
interface StorageSeriesBucket {
  t_ms?: number;
  requests?: number;
  errors?: number;
  bytes_in?: number;
  bytes_out?: number;
  latency_sum_ms?: number;
  latency_max_ms?: number;
}

interface StorageMetricEvent {
  t_ms?: number;
  ok?: boolean;
  latency_ms?: number;
  bytes_in?: number;
  bytes_out?: number;
  error?: string;
}

interface StorageDbMetrics extends DbInstanceMetrics {
  series?: StorageSeriesBucket[];
  granularity_ms?: number;
  window_minutes?: number;
  series_sources?: string[];
  events?: StorageMetricEvent[];
}

type TrafficKey = "requests" | "errors" | "bytes_in" | "bytes_out";
type LatencyKey = "avg_latency" | "max_latency";
type MetricKey = TrafficKey | LatencyKey;

interface DbView {
  id: string;
  name: string;
  kind: string;
  color: string;
  metrics: StorageDbMetrics | null;
  error: string | null;
}

interface TimeRow {
  t_ms: number;
  byDb: Record<string, StorageSeriesBucket | null>;
}

interface Delta {
  text: string;
  tone: "default" | "green" | "red";
  hint: string;
  aria: string;
}

const MAX_DATABASES = 12;
const METRICS_POLL_MS = 15_000;
const WINDOWS = {
  "60m": { label: "60m", minutes: 60 },
  "3h": { label: "3h", minutes: 180 },
} as const;
type WindowKey = keyof typeof WINDOWS;
const WINDOW_KEYS: WindowKey[] = ["60m", "3h"];

/** Six measured hues, then their deeper steps so a twelfth database still owns its own line. */
const SERIES_COLORS = [
  "#6366f1", "#22d3ee", "#34d399", "#fbbf24", "#fb7185", "#a78bfa",
  "#4338ca", "#0e7490", "#047857", "#b45309", "#be123c", "#6d28d9",
];

const TRAFFIC: { key: TrafficKey; label: string; format: (value: number) => string }[] = [
  { key: "requests", label: "Requests", format: formatCount },
  { key: "errors", label: "Errors", format: formatCount },
  { key: "bytes_in", label: "Ingress", format: formatBytes },
  { key: "bytes_out", label: "Egress", format: formatBytes },
];
const LATENCY: { key: LatencyKey; label: string }[] = [
  { key: "avg_latency", label: "Average latency" },
  { key: "max_latency", label: "Maximum latency" },
];

const COUNT = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });

function num(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function formatCount(value: number): string {
  return COUNT.format(value);
}

function formatBytes(value: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let size = Math.abs(value);
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return (size < 10 ? size.toFixed(1) : Math.round(size)) + " " + units[unit];
}

function formatMs(value: number): string {
  return (value < 10 ? value.toFixed(1) : Math.round(value)) + " ms";
}

function formatPct(value: number): string {
  return (value * 100).toFixed(1) + "%";
}

function utcLabel(ms: number): string {
  return new Date(ms).toISOString().slice(11, 16);
}

function firstFinite(values: (number | null | undefined)[]): number | null {
  for (const value of values) if (typeof value === "number" && Number.isFinite(value)) return value;
  return null;
}

function valueFor(bucket: StorageSeriesBucket, metric: MetricKey): number | null {
  if (metric === "avg_latency") {
    const sum = num(bucket.latency_sum_ms);
    const requests = num(bucket.requests);
    if (sum === null || requests === null || requests <= 0) return null;
    return sum / requests;
  }
  if (metric === "max_latency") return num(bucket.latency_max_ms);
  return num(bucket[metric]);
}

function halfDelta(values: (number | null)[], positiveIsGood: boolean | null, format: (v: number) => string): Delta {
  const real = values.filter((value): value is number => value !== null && Number.isFinite(value));
  if (real.length < 2) {
    const hint = "Fewer than two measured buckets.";
    return { text: "No comparison", tone: "default", hint, aria: "No comparison: " + hint };
  }
  const middle = Math.floor(real.length / 2);
  const first = real.slice(0, middle);
  const second = real.slice(middle);
  const firstAvg = first.reduce((sum, value) => sum + value, 0) / first.length;
  const secondAvg = second.reduce((sum, value) => sum + value, 0) / second.length;
  const hint = "First half avg " + format(firstAvg) + " · second half avg " + format(secondAvg);
  if (firstAvg === 0) return { text: "No baseline", tone: "default", hint, aria: "No baseline: " + hint };
  const pct = ((secondAvg - firstAvg) / firstAvg) * 100;
  if (!Number.isFinite(pct)) return { text: "No baseline", tone: "default", hint, aria: "No baseline: " + hint };
  const text = (pct >= 0 ? "+" : "−") + Math.abs(pct).toFixed(1) + "%";
  const tone = pct === 0 || positiveIsGood === null ? "default" : (pct > 0) === positiveIsGood ? "green" : "red";
  return { text, tone, hint, aria: "Change " + text + "; " + hint };
}

function rateDelta(errors: (number | null)[], requests: (number | null)[]): Delta {
  const pairs: { errors: number; requests: number }[] = [];
  for (let index = 0; index < errors.length; index += 1) {
    const e = errors[index];
    const r = requests[index];
    if (e !== null && r !== null && r > 0) pairs.push({ errors: e, requests: r });
  }
  if (pairs.length < 2) {
    const hint = "Fewer than two measured buckets with requests.";
    return { text: "No comparison", tone: "default", hint, aria: "No comparison: " + hint };
  }
  const middle = Math.floor(pairs.length / 2);
  const rate = (slice: { errors: number; requests: number }[]) => {
    let e = 0;
    let r = 0;
    for (const pair of slice) {
      e += pair.errors;
      r += pair.requests;
    }
    return r > 0 ? e / r : null;
  };
  const first = rate(pairs.slice(0, middle));
  const second = rate(pairs.slice(middle));
  const hint = "First half " + (first === null ? "—" : formatPct(first)) + " · second half " + (second === null ? "—" : formatPct(second));
  if (first === null || second === null || first === 0) {
    return { text: "No baseline", tone: "default", hint, aria: "No baseline: " + hint };
  }
  const pct = ((second - first) / first) * 100;
  const text = (pct >= 0 ? "+" : "−") + Math.abs(pct).toFixed(1) + "%";
  return { text, tone: pct === 0 ? "default" : pct > 0 ? "red" : "green", hint, aria: "Change " + text + "; " + hint };
}

/** The metrics list is polled from the browser like every other dashboard reading. A change in the
 *  database id list or a Refresh re-arms the interval; a failed read keeps the last good payload. */
function useStorageMetrics(idsKey: string, refreshToken: number) {
  const [state, setState] = useState<Record<string, { metrics: StorageDbMetrics | null; error: string | null }>>({});
  useEffect(() => {
    let cancelled = false;
    let generation = 0;
    let ids: string[] = [];
    try {
      const parsed: unknown = JSON.parse(idsKey);
      if (Array.isArray(parsed)) ids = parsed.filter((id): id is string => typeof id === "string");
    } catch {
      ids = [];
    }
    const load = async () => {
      const myGeneration = ++generation;
      const results = await Promise.all(
        ids.map(async (id) => {
          try {
            const metrics = await apiGet<StorageDbMetrics>(
              "/v1/databases/" + encodeURIComponent(id) + "/metrics",
              { fresh: true },
            );
            return [id, { metrics, error: null }] as const;
          } catch (e) {
            return [id, { metrics: null, error: String(e) }] as const;
          }
        }),
      );
      if (cancelled || myGeneration !== generation) return;
      setState((previous) => {
        const next: Record<string, { metrics: StorageDbMetrics | null; error: string | null }> = {};
        for (const [id, result] of results) {
          const lastGood = previous[id]?.metrics ?? null;
          next[id] = result.metrics
            ? { metrics: result.metrics, error: null }
            : { metrics: lastGood, error: result.error };
        }
        return next;
      });
    };
    void load();
    const tick = () => {
      if (typeof document !== "undefined" && document.visibilityState === "hidden") return;
      void load();
    };
    const onVisible = () => {
      if (typeof document !== "undefined" && document.visibilityState === "visible") void load();
    };
    const timer = setInterval(tick, METRICS_POLL_MS);
    if (typeof document !== "undefined") document.addEventListener("visibilitychange", onVisible);
    return () => {
      cancelled = true;
      clearInterval(timer);
      if (typeof document !== "undefined") document.removeEventListener("visibilitychange", onVisible);
    };
  }, [idsKey, refreshToken]);
  return state;
}

/** A small gradient sparkline. Gaps are unreported buckets, never joined through a zero. */
function Sparkline({ values, color, label }: { values: (number | null)[]; color: string; label: string }) {
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const gradientId = uid + "-spark-fill";
  const real = values.map(num);
  const measured = real.filter((value) => value !== null).length;
  const W = 240;
  const H = 64;
  const PAD = 4;
  let peak = 0;
  for (const value of real) if (value !== null && value > peak) peak = value;
  const max = peak > 0 ? peak : 1;
  const xFor = (index: number) => (real.length > 1 ? PAD + (index / (real.length - 1)) * (W - 2 * PAD) : PAD);
  const yFor = (value: number) => H - PAD - (value / max) * (H - 2 * PAD);
  const segments = chartSegments(real, xFor, yFor);
  return (
    <svg
      viewBox="0 0 240 64"
      preserveAspectRatio="none"
      className="mt-3 h-14 w-full max-w-full"
      role="img"
      aria-label={label + " across " + measured + " measured buckets"}
    >
      <defs>
        <linearGradient id={gradientId} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" style={{ stopColor: color, stopOpacity: 0.42 }} />
          <stop offset="100%" style={{ stopColor: color, stopOpacity: 0.02 }} />
        </linearGradient>
      </defs>
      <line x1={PAD} y1={H - PAD} x2={W - PAD} y2={H - PAD} stroke="hsl(var(--border))" strokeWidth="1" />
      {segments.map((segment) => {
        const points = segment.points;
        if (points.length === 1) {
          return <circle key={segment.start} cx={points[0].x} cy={points[0].y} r="3.5" style={{ fill: color }} />;
        }
        const line = segmentPath(points);
        const first = points[0];
        const last = points[points.length - 1];
        const area = line + " L " + last.x.toFixed(2) + " " + (H - PAD) + " L " + first.x.toFixed(2) + " " + (H - PAD) + " Z";
        return (
          <g key={segment.start}>
            <path d={area} fill={"url(#" + gradientId + ")"} />
            <path d={line} fill="none" style={{ stroke: color }} strokeOpacity="0.16" strokeWidth="7" strokeLinecap="round" strokeLinejoin="round" />
            <path d={line} fill="none" style={{ stroke: color }} strokeWidth="2.25" strokeLinecap="round" strokeLinejoin="round" />
          </g>
        );
      })}
    </svg>
  );
}

function MetricCard({
  label,
  value,
  delta,
  values,
  color,
}: {
  label: string;
  value: string;
  delta: Delta;
  values: (number | null)[];
  color: string;
}) {
  return (
    <Card className="relative min-w-0 w-full max-w-full overflow-hidden p-0">
      <div
        className="absolute inset-x-0 top-0 h-1"
        style={{ background: "linear-gradient(90deg, " + color + ", transparent)" }}
        aria-hidden="true"
      />
      <div className="min-w-0 p-4 sm:p-5">
        <div className="flex min-w-0 items-start justify-between gap-2">
          <div className="min-w-0">
            <div className="min-w-0 break-words text-xs font-medium uppercase leading-tight tracking-wide text-muted">{label}</div>
            <div className="mt-1 min-w-0 break-words text-2xl font-semibold leading-tight tabular-nums text-fg sm:text-3xl">{value}</div>
          </div>
          <Badge tone={delta.tone} className="shrink-0 tabular-nums" aria-label={delta.aria}>
            {delta.text}
          </Badge>
        </div>
        <Sparkline values={values} color={color} label={label} />
        <div className="mt-2 flex min-w-0 items-start justify-between gap-2 text-xs text-secondary">
          <span className="shrink-0">Aggregate buckets</span>
          <span className="min-w-0 break-words text-right tabular-nums">{delta.hint}</span>
        </div>
      </div>
    </Card>
  );
}

function LatencyPanel({
  label,
  value,
  values,
  color,
  note,
}: {
  label: string;
  value: string;
  values: (number | null)[];
  color: string;
  note: string;
}) {
  return (
    <Card className="relative min-w-0 w-full max-w-full overflow-hidden p-0">
      <div
        className="absolute inset-x-0 top-0 h-1"
        style={{ background: "linear-gradient(90deg, " + color + ", transparent)" }}
        aria-hidden="true"
      />
      <div className="min-w-0 p-4 sm:p-5">
        <div className="min-w-0 break-words text-xs font-medium uppercase leading-tight tracking-wide text-muted">{label}</div>
        <div className="mt-1 min-w-0 break-words text-2xl font-semibold leading-tight tabular-nums text-fg sm:text-3xl">{value}</div>
        <Sparkline values={values} color={color} label={label} />
        <p className="mt-2 min-w-0 break-words text-xs text-secondary">{note}</p>
      </div>
    </Card>
  );
}

export default function StorageAnalyticsClient() {
  const [windowKey, setWindowKey] = useState<WindowKey>("60m");
  const [trafficMetric, setTrafficMetric] = useState<TrafficKey>("requests");
  const [latencyMetric, setLatencyMetric] = useState<LatencyKey>("avg_latency");
  const [hiddenKeys, setHiddenKeys] = useState<string[]>([]);
  const [refreshToken, setRefreshToken] = useState(0);
  const { data: databases, error: listError, loading: listLoading, refresh: refreshList } = usePoll<Database[]>("/v1/databases", 5000);

  const ids = useMemo(() => (databases ?? []).slice(0, MAX_DATABASES).map((item) => item.id), [databases]);
  const idsKey = JSON.stringify(ids);
  const metricsById = useStorageMetrics(idsKey, refreshToken);

  const dbs = useMemo<DbView[]>(
    () =>
      ids.map((id, index) => {
        const db = (databases ?? []).find((item) => item.id === id);
        const entry = metricsById[id];
        const metrics = entry?.metrics ?? null;
        return {
          id,
          name: metrics?.name ?? db?.name ?? id,
          kind: metrics?.kind_label ?? db?.kind ?? "—",
          color: SERIES_COLORS[index % SERIES_COLORS.length],
          metrics,
          error: entry?.error ?? null,
        };
      }),
    [ids, databases, metricsById],
  );

  const allRows = useMemo<TimeRow[]>(() => {
    const map = new Map<number, Record<string, StorageSeriesBucket | null>>();
    for (const db of dbs) {
      const list = db.metrics?.series;
      if (!Array.isArray(list)) continue;
      for (const bucket of list) {
        if (!bucket || typeof bucket !== "object") continue;
        const t = num(bucket.t_ms);
        if (t === null) continue;
        let row = map.get(t);
        if (!row) {
          row = {};
          map.set(t, row);
        }
        row[db.id] = bucket;
      }
    }
    return Array.from(map.entries())
      .sort((a, b) => a[0] - b[0])
      .map(([t_ms, byDb]) => ({ t_ms, byDb }));
  }, [dbs]);

  const rows = useMemo(() => {
    if (allRows.length === 0) return [];
    const newest = allRows[allRows.length - 1].t_ms;
    const cutoff = newest - WINDOWS[windowKey].minutes * 60_000;
    return allRows.filter((row) => row.t_ms >= cutoff);
  }, [allRows, windowKey]);

  const seriesIds = useMemo(() => {
    const present = new Set<string>();
    for (const row of rows) for (const id of Object.keys(row.byDb)) present.add(id);
    return dbs.filter((db) => present.has(db.id)).map((db) => db.id);
  }, [rows, dbs]);
  const activeKeys = useMemo(() => seriesIds.filter((id) => !hiddenKeys.includes(id)), [seriesIds, hiddenKeys]);
  const series = useMemo<StorageChartSeries[]>(
    () => dbs.filter((db) => seriesIds.includes(db.id)).map((db) => ({ key: db.id, label: db.name, color: db.color })),
    [dbs, seriesIds],
  );

  const columnsFor = useCallback(
    (metric: MetricKey): StorageChartColumn[] =>
      rows.map((row) => {
        const values: Record<string, number | null> = {};
        for (const db of dbs) {
          const bucket = row.byDb[db.id] ?? null;
          values[db.id] = bucket ? valueFor(bucket, metric) : null;
        }
        return { t_ms: row.t_ms, label: utcLabel(row.t_ms), values };
      }),
    [rows, dbs],
  );
  const trafficColumns = useMemo(() => columnsFor(trafficMetric), [columnsFor, trafficMetric]);
  const latencyColumns = useMemo(() => columnsFor(latencyMetric), [columnsFor, latencyMetric]);

  const averageFor = useCallback(
    (columns: StorageChartColumn[], keys: string[]): (number | null)[] =>
      columns.map((column) => {
        let sum = 0;
        let count = 0;
        for (const key of keys) {
          const value = num(column.values[key]);
          if (value !== null) {
            sum += value;
            count += 1;
          }
        }
        return count > 0 ? sum / count : null;
      }),
    [],
  );
  const trafficAverage = useMemo(() => averageFor(trafficColumns, activeKeys), [averageFor, trafficColumns, activeKeys]);
  const latencyAverage = useMemo(() => averageFor(latencyColumns, activeKeys), [averageFor, latencyColumns, activeKeys]);

  const aggregateFor = useCallback(
    (metric: MetricKey): (number | null)[] =>
      rows.map((row) => {
        let sum = 0;
        let found = false;
        for (const db of dbs) {
          const bucket = row.byDb[db.id];
          if (!bucket) continue;
          const value = valueFor(bucket, metric);
          if (value === null) continue;
          sum += value;
          found = true;
        }
        return found ? sum : null;
      }),
    [rows, dbs],
  );
  const aggRequests = useMemo(() => aggregateFor("requests"), [aggregateFor]);
  const aggErrors = useMemo(() => aggregateFor("errors"), [aggregateFor]);
  const aggBytesIn = useMemo(() => aggregateFor("bytes_in"), [aggregateFor]);
  const aggBytesOut = useMemo(() => aggregateFor("bytes_out"), [aggregateFor]);
  const errorRateSeries = useMemo<(number | null)[]>(
    () => aggRequests.map((requests, index) => {
      const errors = aggErrors[index];
      return errors !== null && requests !== null && requests > 0 ? errors / requests : null;
    }),
    [aggRequests, aggErrors],
  );
  const windowTotal = useMemo(() => {
    const values = aggregateFor(trafficMetric);
    let sum = 0;
    let found = false;
    for (const value of values) {
      if (value !== null) {
        sum += value;
        found = true;
      }
    }
    return found ? sum : null;
  }, [aggregateFor, trafficMetric]);

  const avgLatencySeries = useMemo<(number | null)[]>(
    () =>
      rows.map((row) => {
        let sum = 0;
        let requests = 0;
        for (const db of dbs) {
          const bucket = row.byDb[db.id];
          if (!bucket) continue;
          const s = num(bucket.latency_sum_ms);
          const r = num(bucket.requests);
          if (s === null || r === null || r <= 0) continue;
          sum += s;
          requests += r;
        }
        return requests > 0 ? sum / requests : null;
      }),
    [rows, dbs],
  );
  const maxLatencySeries = useMemo<(number | null)[]>(
    () =>
      rows.map((row) => {
        let peak: number | null = null;
        for (const db of dbs) {
          const bucket = row.byDb[db.id];
          if (!bucket) continue;
          const value = num(bucket.latency_max_ms);
          if (value !== null && (peak === null || value > peak)) peak = value;
        }
        return peak;
      }),
    [rows, dbs],
  );
  const avgLatencyWindow = useMemo(() => {
    let sum = 0;
    let requests = 0;
    for (const row of rows) {
      for (const db of dbs) {
        const bucket = row.byDb[db.id];
        if (!bucket) continue;
        const s = num(bucket.latency_sum_ms);
        const r = num(bucket.requests);
        if (s === null || r === null || r <= 0) continue;
        sum += s;
        requests += r;
      }
    }
    return requests > 0 ? sum / requests : null;
  }, [rows, dbs]);
  const maxLatencyWindow = useMemo(() => {
    let peak: number | null = null;
    for (const value of maxLatencySeries) if (value !== null && (peak === null || value > peak)) peak = value;
    return peak;
  }, [maxLatencySeries]);

  const totals = useMemo(() => {
    let requests = 0;
    let errors = 0;
    let bytesIn = 0;
    let bytesOut = 0;
    let hasRequests = false;
    let hasErrors = false;
    let hasBytesIn = false;
    let hasBytesOut = false;
    for (const db of dbs) {
      const counters = db.metrics?.counters;
      if (!counters) continue;
      const r = num(counters.requests);
      const e = num(counters.errors);
      const i = num(counters.bytes_in);
      const o = num(counters.bytes_out);
      if (r !== null) { requests += r; hasRequests = true; }
      if (e !== null) { errors += e; hasErrors = true; }
      if (i !== null) { bytesIn += i; hasBytesIn = true; }
      if (o !== null) { bytesOut += o; hasBytesOut = true; }
    }
    return {
      requests: hasRequests ? requests : null,
      errors: hasErrors ? errors : null,
      bytesIn: hasBytesIn ? bytesIn : null,
      bytesOut: hasBytesOut ? bytesOut : null,
      errorRate: hasRequests && hasErrors && requests > 0 ? errors / requests : null,
    };
  }, [dbs]);

  const breakdownRows = useMemo<StorageBreakdownRow[]>(
    () =>
      dbs.map((db) => {
        const counters = db.metrics?.counters;
        const requests = counters ? num(counters.requests) : null;
        const bytesIn = counters ? num(counters.bytes_in) : null;
        const bytesOut = counters ? num(counters.bytes_out) : null;
        let share: number | null = null;
        if (requests !== null && totals.requests !== null && totals.requests > 0) share = requests / totals.requests;
        else if (requests === 0 && totals.requests === 0) share = 0;
        const reachability = db.metrics?.reachability;
        return {
          id: db.id,
          name: db.name,
          kind: db.kind,
          requests,
          share,
          bytesIn,
          bytesOut,
          reachability: typeof reachability === "string" && reachability.length > 0 ? reachability : null,
          measured: typeof db.metrics?.measured === "boolean" ? db.metrics.measured : null,
          color: db.color,
        };
      }),
    [dbs, totals.requests],
  );

  const events = useMemo<StorageEventRow[]>(() => {
    const out: StorageEventRow[] = [];
    for (const db of dbs) {
      const list = db.metrics?.events;
      if (!Array.isArray(list)) continue;
      for (let index = 0; index < list.length; index += 1) {
        const event = list[index];
        if (!event || typeof event !== "object") continue;
        const t = num(event.t_ms);
        if (t === null) continue;
        out.push({
          id: db.id + ":" + t + ":" + index,
          t_ms: t,
          dbName: db.name,
          dbColor: db.color,
          ok: typeof event.ok === "boolean" ? event.ok : null,
          latency_ms: num(event.latency_ms),
          bytes_in: num(event.bytes_in),
          bytes_out: num(event.bytes_out),
          error: typeof event.error === "string" && event.error.length > 0 ? event.error : null,
        });
      }
    }
    out.sort((a, b) => b.t_ms - a.t_ms);
    return out;
  }, [dbs]);

  const sourceCaption = useMemo(() => {
    const gran = firstFinite(dbs.map((db) => num(db.metrics?.granularity_ms)));
    const reportedWindow = firstFinite(dbs.map((db) => num(db.metrics?.window_minutes)));
    const sources = new Set<string>();
    for (const db of dbs) {
      const list = db.metrics?.series_sources;
      if (!Array.isArray(list)) continue;
      for (const source of list) if (typeof source === "string" && source.length > 0) sources.add(source);
    }
    const bucketText = gran !== null
      ? (gran >= 60_000 ? Math.round(gran / 60_000) + " min buckets" : Math.round(gran / 1000) + " s buckets")
      : "Bucket width not reported";
    const windowText = reportedWindow !== null ? reportedWindow + " min window reported" : "Window length not reported";
    const allSources = Array.from(sources);
    const sourceText = allSources.length
      ? "Series sources: " + allSources.slice(0, 6).join(", ") + (allSources.length > 6 ? " +" + (allSources.length - 6) + " more" : "")
      : "Series sources not reported";
    return bucketText + " · " + windowText + " · " + sourceText + ". The window selector narrows the buckets the node returned.";
  }, [dbs]);

  const lastObservedMs = useMemo(() => {
    let latest: number | null = null;
    for (const db of dbs) {
      const value = num(db.metrics?.observed_at_ms);
      if (value !== null && (latest === null || value > latest)) latest = value;
    }
    return latest;
  }, [dbs]);
  const metricErrors = dbs.filter((db) => db.error !== null);
  const metricsPending = dbs.length > 0 && dbs.every((db) => db.metrics === null && db.error === null);
  const traffic = TRAFFIC.find((item) => item.key === trafficMetric) ?? TRAFFIC[0];
  const latency = LATENCY.find((item) => item.key === latencyMetric) ?? LATENCY[0];
  const windowLabel = WINDOWS[windowKey].label + " window";
  const trafficCaption =
    "Each coloured line is one database's reported " + traffic.label.toLowerCase() +
    " per bucket. The table view carries the plain sum across the selected databases.";
  const latencyCaption = latencyMetric === "avg_latency"
    ? "Computed as latency_sum_ms / requests for each database and bucket. Only reported buckets are drawn."
    : "The largest latency_max_ms each database reported per bucket. Only reported buckets are drawn.";

  const refreshAll = useCallback(() => {
    refreshList();
    setRefreshToken((value) => value + 1);
  }, [refreshList]);

  function toggleDatabase(key: string) {
    setHiddenKeys((previous) =>
      previous.includes(key) ? previous.filter((item) => item !== key) : previous.concat([key]),
    );
  }

  return (
    <div className="relative min-w-0 w-full max-w-full overflow-x-hidden px-4 py-5 sm:px-6">
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-x-0 top-0 h-44"
        style={{
          background: "linear-gradient(180deg, rgba(99,102,241,0.14), rgba(34,211,238,0.05) 45%, transparent)",
          maskImage: "linear-gradient(90deg, transparent, black 12%, black 88%, transparent)",
          WebkitMaskImage: "linear-gradient(90deg, transparent, black 12%, black 88%, transparent)",
        }}
      />
      <div className="relative min-w-0 w-full max-w-full">
        <PageHeader
          title="Storage analytics"
          desc="Measured requests, traffic, latency and request events across all databases returned for this tenant."
          action={
            <>
              <Badge tone="purple" className="min-w-0 max-w-full">
                <span className="min-w-0 break-words">
                  {lastObservedMs === null ? "Waiting for measurements" : "Last observation " + timeAgo(lastObservedMs)}
                </span>
              </Badge>
              <Button variant="outline" aria-label="Refresh the database list and every database's metrics" className="min-h-[44px] min-w-0" onClick={refreshAll}>
                <RefreshCw className="h-4 w-4 shrink-0" aria-hidden="true" />
                <span className="min-w-0 break-words">Refresh</span>
              </Button>
              <Link
                href="/storage"
                className="inline-flex min-h-[44px] min-w-0 max-w-full items-center gap-2 rounded-md border border-border px-3 text-sm font-medium text-fg hover:bg-subtle"
              >
                <ArrowLeft className="h-4 w-4 shrink-0" aria-hidden="true" />
                <span className="min-w-0 break-words">Databases</span>
              </Link>
            </>
          }
        />

        <div role="group" aria-label="Window" className="mb-4 flex min-w-0 flex-wrap items-center gap-2">
          <span className="shrink-0 text-xs font-medium uppercase tracking-wide text-muted">Window</span>
          {WINDOW_KEYS.map((key) => (
            <Button
              key={key}
              variant={key === windowKey ? "default" : "outline"}
              aria-pressed={key === windowKey}
              className="min-h-[44px] min-w-0 px-3"
              style={key === windowKey ? { background: "linear-gradient(90deg, #6366f1, #22d3ee)", color: "#ffffff", borderColor: "transparent" } : undefined}
              onClick={() => setWindowKey(key)}
            >
              {WINDOWS[key].label}
            </Button>
          ))}
          <span className="min-w-0 break-words text-xs text-secondary">
            The database list polls every 5 s; each database&apos;s metrics poll every {METRICS_POLL_MS / 1000} s.
          </span>
        </div>

        {listError ? <div className="mb-4"><StateNotice>Couldn&apos;t load the database list: {listError}</StateNotice></div> : null}
        {databases && databases.length > MAX_DATABASES ? (
          <div className="mb-4">
            <StateNotice tone="muted">
              Showing metrics for the first {MAX_DATABASES} of {databases.length} databases returned.
            </StateNotice>
          </div>
        ) : null}
        {metricsPending ? (
          <div className="mb-4">
            <StateNotice tone="muted">
              Waiting for the first metrics response from {dbs.length} {dbs.length === 1 ? "database" : "databases"}.
            </StateNotice>
          </div>
        ) : null}
        {metricErrors.length > 0 ? (
          <div className="mb-4">
            <StateNotice>
              Metrics did not answer for {metricErrors.length} {metricErrors.length === 1 ? "database" : "databases"}: {metricErrors.map((db) => db.name).join(", ")}.{" "}
              {metricErrors.some((db) => db.metrics !== null)
                ? "The last successful reading is still shown."
                : "No reading is available for those databases."}
            </StateNotice>
          </div>
        ) : null}

        {!databases && listLoading ? (
          <Card className="min-w-0 w-full max-w-full p-5">
            <p className="min-w-0 break-words text-sm text-secondary">Waiting for the database list.</p>
          </Card>
        ) : databases && databases.length === 0 ? (
          <StateNotice tone="muted">No databases are returned for this tenant.</StateNotice>
        ) : !databases ? (
          <StateNotice tone="muted">The database list has not returned data.</StateNotice>
        ) : (
          <div className="flex min-w-0 w-full max-w-full flex-col gap-4" aria-busy={listLoading}>
            <Card className="min-w-0 w-full max-w-full p-4 sm:p-5">
              <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
                <h2 className="min-w-0 break-words text-sm font-semibold text-fg">Databases with measured buckets</h2>
                <Badge tone="blue" className="shrink-0 tabular-nums">
                  {windowTotal === null ? "No bucket total" : "Σ " + traffic.format(windowTotal) + " across all databases in " + WINDOWS[windowKey].label}
                </Badge>
              </div>
              <p className="mt-1 min-w-0 break-words text-xs leading-snug text-secondary">{sourceCaption}</p>
              <div className="mt-3 flex min-w-0 flex-wrap gap-2" role="group" aria-label="Database series toggles">
                {series.map((item) => {
                  const isActive = activeKeys.includes(item.key);
                  return (
                    <Button
                      key={item.key}
                      variant="outline"
                      aria-pressed={isActive}
                      className="min-h-[44px] min-w-0 max-w-full gap-2 px-3 text-xs"
                      style={isActive
                        ? { borderColor: item.color, background: "linear-gradient(90deg, " + item.color + "2E, transparent)", color: "hsl(var(--fg))" }
                        : { borderColor: item.color, opacity: 0.55 }}
                      onClick={() => toggleDatabase(item.key)}
                    >
                      <span className="h-0.5 w-4 shrink-0 rounded-full" style={{ background: item.color }} aria-hidden="true" />
                      <span className="min-w-0 break-words">{item.label}</span>
                    </Button>
                  );
                })}
                <span className="inline-flex min-h-[44px] min-w-0 items-center gap-2 rounded-md border border-border px-3 text-xs text-secondary">
                  <svg width="20" height="8" viewBox="0 0 20 8" aria-hidden="true">
                    <line x1="0" y1="4" x2="20" y2="4" stroke="hsl(var(--fg))" strokeWidth="2" strokeDasharray="3 4" />
                  </svg>
                  <span className="min-w-0 break-words">Average (dotted)</span>
                </span>
              </div>
              {series.length === 0 ? (
                <p className="mt-3 min-w-0 break-words text-sm text-secondary">
                  No database returned a measured bucket. A node that predates the series payload reports counters only; those counters are shown below.
                </p>
              ) : null}
              <p className="mt-3 min-w-0 break-words text-xs text-secondary">
                {series.length} of {dbs.length} databases reported buckets in this window. Databases without measured buckets are still listed in the table below.
              </p>
            </Card>

            <div className="grid min-w-0 grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-5">
              <MetricCard label="Requests" value={totals.requests === null ? "—" : formatCount(totals.requests)} delta={halfDelta(aggRequests, null, formatCount)} values={aggRequests} color={SERIES_COLORS[0]} />
              <MetricCard label="Errors" value={totals.errors === null ? "—" : formatCount(totals.errors)} delta={halfDelta(aggErrors, false, formatCount)} values={aggErrors} color={SERIES_COLORS[4]} />
              <MetricCard label="Error rate" value={totals.errorRate === null ? "—" : formatPct(totals.errorRate)} delta={rateDelta(aggErrors, aggRequests)} values={errorRateSeries} color={SERIES_COLORS[3]} />
              <MetricCard label="Ingress" value={totals.bytesIn === null ? "—" : formatBytes(totals.bytesIn)} delta={halfDelta(aggBytesIn, null, formatBytes)} values={aggBytesIn} color={SERIES_COLORS[2]} />
              <MetricCard label="Egress" value={totals.bytesOut === null ? "—" : formatBytes(totals.bytesOut)} delta={halfDelta(aggBytesOut, null, formatBytes)} values={aggBytesOut} color={SERIES_COLORS[1]} />
            </div>
            <p className="min-w-0 break-words text-xs text-secondary">
              Card values are the sum of the counters each database reported. A delta compares the first half with the second half of the measured aggregate series; a bucket a database did not report is absent from both.
            </p>

            <div role="group" aria-label="Traffic chart metric" className="flex min-w-0 flex-wrap items-center gap-2">
              <span className="shrink-0 text-xs font-medium uppercase tracking-wide text-muted">Traffic chart</span>
              {TRAFFIC.map((item) => (
                <Button
                  key={item.key}
                  variant={item.key === trafficMetric ? "default" : "outline"}
                  aria-pressed={item.key === trafficMetric}
                  className="min-h-[44px] min-w-0 px-3"
                  onClick={() => setTrafficMetric(item.key)}
                >
                  {item.label}
                </Button>
              ))}
            </div>
            <StorageAnalyticsChart
              title={traffic.label + " by database"}
              caption={trafficCaption}
              columns={trafficColumns}
              series={series}
              activeKeys={activeKeys}
              average={trafficAverage}
              format={traffic.format}
              windowLabel={windowLabel}
              showTotal
              emptyText="No database returned a bucket with this metric in the selected window."
            />

            <div role="group" aria-label="Latency chart metric" className="flex min-w-0 flex-wrap items-center gap-2">
              <span className="shrink-0 text-xs font-medium uppercase tracking-wide text-muted">Latency chart</span>
              {LATENCY.map((item) => (
                <Button
                  key={item.key}
                  variant={item.key === latencyMetric ? "default" : "outline"}
                  aria-pressed={item.key === latencyMetric}
                  className="min-h-[44px] min-w-0 px-3"
                  onClick={() => setLatencyMetric(item.key)}
                >
                  {item.label}
                </Button>
              ))}
            </div>
            <StorageAnalyticsChart
              title={latency.label + " by database"}
              caption={latencyCaption}
              columns={latencyColumns}
              series={series}
              activeKeys={activeKeys}
              average={latencyAverage}
              format={formatMs}
              windowLabel={windowLabel}
              emptyText="No database returned latency readings in the selected window."
            />

            <div className="grid min-w-0 grid-cols-1 gap-3 sm:grid-cols-2">
              <LatencyPanel
                label="Average latency"
                value={avgLatencyWindow === null ? "—" : formatMs(avgLatencyWindow)}
                values={avgLatencySeries}
                color={SERIES_COLORS[5]}
                note="Computed from latency_sum_ms and requests in the reported buckets."
              />
              <LatencyPanel
                label="Maximum latency"
                value={maxLatencyWindow === null ? "—" : formatMs(maxLatencyWindow)}
                values={maxLatencySeries}
                color={SERIES_COLORS[4]}
                note="The largest latency_max_ms the node reported in this window."
              />
            </div>
            <p className="min-w-0 break-words text-xs text-secondary">
              The metrics endpoint does not report percentiles, so these panels report a computed mean and the largest observed per-bucket value.
            </p>

            <StorageAnalyticsBreakdown rows={breakdownRows} formatCount={formatCount} formatBytes={formatBytes} />
            <StorageAnalyticsEvents events={events} formatBytes={formatBytes} />
          </div>
        )}
      </div>
    </div>
  );
}
