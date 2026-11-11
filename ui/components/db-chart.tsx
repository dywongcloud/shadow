"use client";

import { useId, useMemo, useState, type KeyboardEvent, type PointerEvent } from "react";
import { Button, Card, StateNotice } from "@/components/ui";
import type { DbInstanceMetrics, DbMetricBucket } from "@/lib/api";

/** Scoped color steps. The six `-lit` values are the requested luminous hues (gradient stops,
 *  glow strokes, top strips); the `color` steps are re-stepped for a 2px stroke to pass the
 *  lightness, CVD-separation and 3:1 contrast checks on a light surface. Dark mode gets its own
 *  selected steps, never an automatic flip. Both sets were measured with validate_palette.js. */
export const DB_PALETTE_CSS = [
  ".hive-db-observe {",
  "  --db-indigo: #4239c2; --db-cyan: #1296aa; --db-emerald: #0c8d63;",
  "  --db-amber: #826001; --db-rose: #9d1239; --db-violet: #896cd8;",
  "  --db-indigo-lit: #6366f1; --db-cyan-lit: #22d3ee; --db-emerald-lit: #34d399;",
  "  --db-amber-lit: #fbbf24; --db-rose-lit: #fb7185; --db-violet-lit: #a78bfa;",
  "}",
  ".dark .hive-db-observe {",
  "  --db-indigo: #4d4ad3; --db-cyan: #149db2; --db-emerald: #07976a;",
  "  --db-amber: #8c6800; --db-rose: #ae2846; --db-violet: #8163ce;",
  "}",
  "@media (forced-colors: active) {",
  "  .hive-db-observe .db-fill { background-image: repeating-linear-gradient(45deg, CanvasText 0 1px, transparent 1px 6px) !important; }",
  "  .hive-db-observe .db-area { fill: none !important; stroke: CanvasText; stroke-dasharray: 3 4; }",
  "}",
].join("\n");

export const DB_COLORS = {
  indigo: { color: "var(--db-indigo, #4239c2)", lit: "var(--db-indigo-lit, #6366f1)" },
  cyan: { color: "var(--db-cyan, #1296aa)", lit: "var(--db-cyan-lit, #22d3ee)" },
  emerald: { color: "var(--db-emerald, #0c8d63)", lit: "var(--db-emerald-lit, #34d399)" },
  amber: { color: "var(--db-amber, #826001)", lit: "var(--db-amber-lit, #fbbf24)" },
  rose: { color: "var(--db-rose, #9d1239)", lit: "var(--db-rose-lit, #fb7185)" },
  violet: { color: "var(--db-violet, #896cd8)", lit: "var(--db-violet-lit, #a78bfa)" },
} as const;

export type DbColorRole = keyof typeof DB_COLORS;
export const ABSENT = "—";

const COUNT = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });
const PLAIN = new Intl.NumberFormat("en-US");

/** Only a finite numeric field is a measurement. A missing optional key stays absent, never a
 *  fabricated zero: the dashboard rolls independently of the nodes. */
export function dbFinite(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

export function dbCount(v: number | null): string {
  return v === null ? ABSENT : COUNT.format(v);
}

export function dbPlain(v: number | null): string {
  return v === null ? ABSENT : PLAIN.format(v);
}

export function dbBytes(v: number | null): string {
  if (v === null) return ABSENT;
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let n = v;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i += 1;
  }
  return `${n < 10 ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
}

export function dbMs(v: number | null): string {
  if (v === null) return ABSENT;
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ms`;
}

/** Server-derived timestamps are formatted as UTC, so a server render and a client render agree
 *  (React #418). A malformed or absent time is stated as absent, never as epoch zero. */
export function dbUtc(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms) || ms <= 0) return "time not reported";
  const iso = new Date(ms).toISOString();
  return `${iso.slice(0, 19).replace("T", " ")} UTC`;
}

export function dbUtcShort(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms) || ms <= 0) return ABSENT;
  const iso = new Date(ms).toISOString();
  return `${iso.slice(11, 16)} UTC`;
}

const W = 1000;
const H = 280;
const LEFT = 62;
const RIGHT = 16;
const TOP = 16;
const BOTTOM = 34;
const PLOT_W = W - LEFT - RIGHT;
const PLOT_H = H - TOP - BOTTOM;
const TICK_FRACTIONS = [0, 0.25, 0.5, 0.75, 1];

type Pt = { x: number; y: number };

/** A curve passes THROUGH every measured point, never above or below the two real values: the
 *  control heights are clamped to the segment's own range. No point is invented. */
function smoothPath(points: Pt[]): string {
  if (points.length === 0) return "";
  if (points.length === 1) return `M ${points[0].x.toFixed(2)} ${points[0].y.toFixed(2)}`;
  let d = `M ${points[0].x.toFixed(2)} ${points[0].y.toFixed(2)}`;
  for (let i = 0; i < points.length - 1; i += 1) {
    const p = points[i];
    const q = points[i + 1];
    const prev = points[Math.max(0, i - 1)];
    const next = points[Math.min(points.length - 1, i + 2)];
    const dx = (q.x - p.x) / 3;
    const lo = Math.min(p.y, q.y);
    const hi = Math.max(p.y, q.y);
    const c1y = Math.min(hi, Math.max(lo, p.y + (q.y - prev.y) / 6));
    const c2y = Math.min(hi, Math.max(lo, q.y - (next.y - p.y) / 6));
    d += ` C ${(p.x + dx).toFixed(2)} ${c1y.toFixed(2)} ${(q.x - dx).toFixed(2)} ${c2y.toFixed(2)} ${q.x.toFixed(2)} ${q.y.toFixed(2)}`;
  }
  return d;
}

function niceMax(raw: number): number {
  if (!(raw > 0)) return 1;
  const exponent = Math.floor(Math.log10(raw));
  const base = 10 ** exponent;
  const scaled = raw / base;
  const step = [1, 2, 2.5, 5, 10].find((candidate) => scaled <= candidate) ?? 10;
  return step * base;
}

interface PanelSeries {
  key: string;
  label: string;
  role: DbColorRole;
  /** Reads one real bucket, or arithmetic on fields of that same bucket. */
  pick: (b: DbMetricBucket) => number;
}

interface PanelSpec {
  id: string;
  title: string;
  unit: string;
  caption: string;
  format: (v: number) => string;
  series: PanelSeries[];
  averageKey: string;
  averageRole: DbColorRole;
}

const PANELS: PanelSpec[] = [
  {
    id: "requests",
    title: "Requests",
    unit: "req",
    caption: "Successful is requests minus errors in the same measured bucket.",
    format: (v) => `${dbCount(v)} req`,
    averageKey: "requests",
    averageRole: "amber",
    series: [
      { key: "requests", label: "Requests", role: "indigo", pick: (b) => b.requests },
      { key: "successful", label: "Successful", role: "emerald", pick: (b) => Math.max(0, b.requests - b.errors) },
      { key: "errors", label: "Errors", role: "rose", pick: (b) => b.errors },
    ],
  },
  {
    id: "errors",
    title: "Errors",
    unit: "err",
    caption: "Errors against all requests measured in the same bucket.",
    format: (v) => `${dbCount(v)} err`,
    averageKey: "errors",
    averageRole: "cyan",
    series: [
      { key: "errors", label: "Errors", role: "rose", pick: (b) => b.errors },
      { key: "requests", label: "Requests", role: "indigo", pick: (b) => b.requests },
    ],
  },
  {
    id: "bandwidth",
    title: "Bandwidth in / out",
    unit: "B",
    caption: "Total is ingress plus egress from the same measured bucket.",
    format: dbBytes,
    averageKey: "total",
    averageRole: "amber",
    series: [
      { key: "bytes_in", label: "Ingress", role: "cyan", pick: (b) => b.bytes_in },
      { key: "bytes_out", label: "Egress", role: "violet", pick: (b) => b.bytes_out },
      { key: "total", label: "Total", role: "emerald", pick: (b) => b.bytes_in + b.bytes_out },
    ],
  },
  {
    id: "latency",
    title: "Latency",
    unit: "ms",
    caption: "Average is latency_sum_ms divided by requests in that bucket; a bucket with no requests reads a real zero.",
    format: dbMs,
    averageKey: "avg",
    averageRole: "cyan",
    series: [
      { key: "avg", label: "Average", role: "amber", pick: (b) => (b.requests > 0 ? b.latency_sum_ms / b.requests : 0) },
      { key: "max", label: "Max", role: "rose", pick: (b) => b.latency_max_ms },
    ],
  },
];

/** A bucket is plotted only when every field the panel reads was actually reported. A partial
 *  reading is dropped as a whole, never filled with a zero that the node did not measure. */
function cleanBuckets(raw: unknown): { buckets: DbMetricBucket[]; omitted: number } {
  const list = Array.isArray(raw) ? raw : [];
  const buckets: DbMetricBucket[] = [];
  let omitted = 0;
  for (const item of list) {
    if (!item || typeof item !== "object") {
      omitted += 1;
      continue;
    }
    const b = item as Record<string, unknown>;
    const fields = ["t_ms", "requests", "errors", "bytes_in", "bytes_out", "latency_sum_ms", "latency_max_ms"];
    const values = fields.map((key) => dbFinite(b[key]));
    if (values.some((v) => v === null) || (values[0] as number) <= 0) {
      omitted += 1;
      continue;
    }
    buckets.push({
      t_ms: values[0] as number,
      requests: values[1] as number,
      errors: values[2] as number,
      bytes_in: values[3] as number,
      bytes_out: values[4] as number,
      latency_sum_ms: values[5] as number,
      latency_max_ms: values[6] as number,
    });
  }
  buckets.sort((a, b) => a.t_ms - b.t_ms);
  return { buckets, omitted };
}

function DbChartPanel({
  panel,
  buckets,
  stepMs,
  windowLabel,
}: {
  panel: PanelSpec;
  buckets: DbMetricBucket[];
  stepMs: number;
  windowLabel: string;
}) {
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const tooltipId = `${uid}-tip`;
  const tableId = `${uid}-table`;
  const [active, setActive] = useState<string[]>(panel.series.map((s) => s.key));
  const [showAverage, setShowAverage] = useState(true);
  const [hover, setHover] = useState<number | null>(null);
  const [tableOpen, setTableOpen] = useState(false);
  const shown = panel.series.filter((s) => active.includes(s.key));
  const hasData = buckets.length > 0 && shown.length > 0;

  const values = useMemo(() => {
    return buckets.map((b) => {
      const row: Record<string, number> = {};
      for (const s of panel.series) row[s.key] = s.pick(b);
      return row;
    });
  }, [buckets, panel]);

  const average = useMemo(() => {
    if (buckets.length === 0) return null;
    let sum = 0;
    for (const row of values) sum += row[panel.averageKey] ?? 0;
    return sum / values.length;
  }, [buckets.length, values, panel.averageKey]);

  const minT = buckets.length ? buckets[0].t_ms : 0;
  const maxT = buckets.length ? buckets[buckets.length - 1].t_ms : 1;
  const span = Math.max(1, maxT - minT);
  const xFor = (t: number) => LEFT + ((t - minT) / span) * PLOT_W;
  let peak = 0;
  for (const row of values) for (const s of shown) peak = Math.max(peak, row[s.key] ?? 0);
  if (showAverage && average !== null) peak = Math.max(peak, average);
  const top = niceMax(peak);
  const yFor = (v: number) => TOP + (1 - Math.max(0, v) / top) * PLOT_H;
  const baseline = yFor(0);

  /** A gap in the series stays a gap: a path never bridges two buckets farther apart than 1.5
   *  measured steps, so no line implies a value in a bucket the node did not return. */
  const segments = useMemo(() => {
    const out: number[][] = [];
    let current: number[] = [];
    for (let i = 0; i < buckets.length; i += 1) {
      if (i > 0 && buckets[i].t_ms - buckets[i - 1].t_ms > stepMs * 1.5) {
        if (current.length) out.push(current);
        current = [];
      }
      current.push(i);
    }
    if (current.length) out.push(current);
    return out;
  }, [buckets, stepMs]);

  const xLabels = useMemo(() => {
    if (!buckets.length) return [];
    const wanted = Math.min(5, buckets.length);
    const idx = Array.from(new Set(Array.from({ length: wanted }, (_, i) => Math.round((i * (buckets.length - 1)) / Math.max(1, wanted - 1)))));
    return idx;
  }, [buckets]);

  function toggle(key: string) {
    setActive((prev) => {
      if (prev.includes(key)) return prev.length === 1 ? prev : prev.filter((k) => k !== key);
      return panel.series.filter((s) => prev.includes(s.key) || s.key === key).map((s) => s.key);
    });
  }

  function nearest(clientX: number, rect: DOMRect): number {
    if (!buckets.length || rect.width <= 0) return 0;
    const fraction = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    const viewX = fraction * W;
    const targetT = minT + ((viewX - LEFT) / PLOT_W) * span;
    let best = 0;
    let bestDistance = Infinity;
    for (let i = 0; i < buckets.length; i += 1) {
      const distance = Math.abs(buckets[i].t_ms - targetT);
      if (distance < bestDistance) {
        bestDistance = distance;
        best = i;
      }
    }
    return best;
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (!buckets.length) return;
    if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) {
      event.preventDefault();
      const base = hover ?? buckets.length - 1;
      if (event.key === "Home") setHover(0);
      else if (event.key === "End") setHover(buckets.length - 1);
      else if (event.key === "ArrowLeft") setHover(Math.max(0, base - 1));
      else setHover(Math.min(buckets.length - 1, base + 1));
    }
  }

  const hovered = hover !== null && hover >= 0 && hover < buckets.length ? buckets[hover] : null;
  const avgColor = DB_COLORS[panel.averageRole];
  const gradientId = (key: string) => `${uid}-fill-${key}`;

  return (
    <Card className="min-w-0 w-full max-w-full p-0" aria-label={`${panel.title} measured series, ${windowLabel}`}>
      <div className="flex min-w-0 flex-col gap-2 border-b border-border px-4 py-3 sm:flex-row sm:items-start sm:justify-between sm:px-5">
        <div className="min-w-0">
          <h3 className="min-w-0 break-words text-sm font-semibold text-fg">{panel.title}</h3>
          <p className="mt-1 min-w-0 break-words text-xs text-secondary">{panel.caption}</p>
        </div>
        <span className="shrink-0 text-xs tabular-nums text-muted">{buckets.length} measured buckets</span>
      </div>

      <div aria-label="Series toggles" className="flex min-w-0 flex-wrap gap-2 px-4 py-3 sm:px-5">
        {panel.series.map((s) => {
          const isActive = active.includes(s.key);
          const role = DB_COLORS[s.role];
          const last = buckets.length ? s.pick(buckets[buckets.length - 1]) : null;
          return (
            <Button
              key={s.key}
              variant={isActive ? "default" : "outline"}
              aria-pressed={isActive}
              className="min-h-[44px] min-w-0 max-w-full gap-2 px-3 text-xs"
              style={isActive
                ? { background: `linear-gradient(90deg, ${role.lit}, ${role.color})`, color: "#ffffff", borderColor: "transparent" }
                : { borderColor: role.color }}
              onClick={() => toggle(s.key)}
            >
              <span className="h-0.5 w-5 shrink-0 rounded-full" style={{ background: role.color }} aria-hidden="true" />
              <span className="min-w-0 break-words">{s.label}</span>
              {last !== null && <strong className="shrink-0 tabular-nums">{panel.format(last)}</strong>}
            </Button>
          );
        })}
        {average !== null && (
          <Button
            variant={showAverage ? "outline" : "ghost"}
            aria-pressed={showAverage}
            className="min-h-[44px] min-w-0 max-w-full gap-2 px-3 text-xs"
            style={showAverage ? { borderColor: avgColor.color } : undefined}
            onClick={() => setShowAverage((v) => !v)}
          >
            <span
              className="h-0 w-5 shrink-0 border-t-2 border-dotted"
              style={{ borderColor: avgColor.color }}
              aria-hidden="true"
            />
            <span className="min-w-0 break-words">Average</span>
            <strong className="shrink-0 tabular-nums">{panel.format(average)}</strong>
          </Button>
        )}
      </div>

      {!hasData ? (
        <div className="flex min-h-[180px] min-w-0 items-center gap-3 px-4 pb-5 sm:px-5">
          <span
            className="db-fill h-14 w-1.5 shrink-0 rounded-full"
            style={{ background: "linear-gradient(180deg, var(--db-indigo-lit, #6366f1), var(--db-emerald-lit, #34d399))" }}
            aria-hidden="true"
          />
          <p className="min-w-0 break-words text-sm text-secondary">
            {buckets.length === 0
              ? "No measured buckets are returned for this window, so no curve is drawn."
              : "Select at least one series to draw."}
          </p>
        </div>
      ) : (
        <>
          <div
            className="relative mx-4 mb-2 h-[220px] min-w-0 w-auto max-w-full overflow-hidden rounded-lg border border-border bg-card sm:mx-5 sm:h-[260px]"
            role="group"
            tabIndex={0}
            aria-label={`${panel.title} chart. Use arrow keys to inspect measured buckets.`}
            aria-describedby={hovered ? tooltipId : undefined}
            style={{ touchAction: "pan-y" }}
            onPointerMove={(event: PointerEvent<HTMLDivElement>) => setHover(nearest(event.clientX, event.currentTarget.getBoundingClientRect()))}
            onPointerLeave={() => setHover(null)}
            onFocus={() => setHover(buckets.length - 1)}
            onBlur={() => setHover(null)}
            onKeyDown={onKeyDown}
          >
            <div className="pointer-events-none absolute left-2 top-[16px] bottom-[34px] z-10 flex flex-col justify-between text-[10px] tabular-nums text-muted">
              {TICK_FRACTIONS.slice().reverse().map((f) => (
                <span key={f} className="min-w-0 whitespace-nowrap">{panel.format(top * f)}</span>
              ))}
            </div>
            <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className="absolute inset-0 h-full w-full" aria-hidden="true">
              <defs>
                {shown.map((s) => {
                  const role = DB_COLORS[s.role];
                  return (
                    <linearGradient key={s.key} id={gradientId(s.key)} x1="0" y1="0" x2="0" y2="1">
                      <stop offset="0%" style={{ stopColor: role.lit, stopOpacity: 0.32 }} />
                      <stop offset="100%" style={{ stopColor: role.color, stopOpacity: 0.02 }} />
                    </linearGradient>
                  );
                })}
              </defs>
              {TICK_FRACTIONS.map((f) => (
                <line
                  key={`y-${f}`}
                  x1={LEFT}
                  x2={W - RIGHT}
                  y1={yFor(top * f)}
                  y2={yFor(top * f)}
                  stroke="hsl(var(--border))"
                  strokeWidth="1"
                  strokeDasharray={f === 0 ? undefined : "2 7"}
                  vectorEffect="non-scaling-stroke"
                />
              ))}
              {xLabels.map((i) => (
                <line
                  key={`x-${i}`}
                  x1={xFor(buckets[i].t_ms)}
                  x2={xFor(buckets[i].t_ms)}
                  y1={TOP}
                  y2={H - BOTTOM}
                  stroke="hsl(var(--border))"
                  strokeWidth="1"
                  strokeDasharray="2 9"
                  vectorEffect="non-scaling-stroke"
                />
              ))}
              {showAverage && average !== null && (
                <line
                  x1={LEFT}
                  x2={W - RIGHT}
                  y1={yFor(average)}
                  y2={yFor(average)}
                  style={{ stroke: avgColor.color }}
                  strokeWidth="2"
                  strokeDasharray="3 7"
                  vectorEffect="non-scaling-stroke"
                />
              )}
              {shown.map((s) => {
                const role = DB_COLORS[s.role];
                return segments.map((segment) => {
                  const points = segment.map((i) => ({ x: xFor(buckets[i].t_ms), y: yFor(values[i][s.key] ?? 0) }));
                  const line = smoothPath(points);
                  if (!line) return null;
                  const area = points.length > 1
                    ? `${line} L ${points[points.length - 1].x.toFixed(2)} ${baseline.toFixed(2)} L ${points[0].x.toFixed(2)} ${baseline.toFixed(2)} Z`
                    : "";
                  return (
                    <g key={`${s.key}-${segment[0]}`}>
                      {area && <path className="db-area" d={area} fill={`url(#${gradientId(s.key)})`} />}
                      <path d={line} fill="none" style={{ stroke: role.lit }} strokeOpacity="0.18" strokeWidth="9" strokeLinecap="round" strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
                      <path d={line} fill="none" style={{ stroke: role.color }} strokeWidth="2.25" strokeLinecap="round" strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
                    </g>
                  );
                });
              })}
              {segments.map((segment) => (
                <g key={`dots-${segment[0]}`}>
                  {segment.length === 1 && shown.map((s) => (
                    <circle
                      key={`single-${s.key}-${segment[0]}`}
                      cx={xFor(buckets[segment[0]].t_ms)}
                      cy={yFor(values[segment[0]][s.key] ?? 0)}
                      r="4"
                      style={{ fill: DB_COLORS[s.role].color, stroke: "hsl(var(--card))" }}
                      strokeWidth="2"
                      vectorEffect="non-scaling-stroke"
                    />
                  ))}
                </g>
              ))}
              {hovered && (
                <g>
                  <line x1={xFor(hovered.t_ms)} x2={xFor(hovered.t_ms)} y1={TOP} y2={H - BOTTOM} stroke="hsl(var(--border-strong))" strokeWidth="1" strokeDasharray="3 4" vectorEffect="non-scaling-stroke" />
                  {shown.map((s) => (
                    <circle
                      key={`hover-${s.key}`}
                      cx={xFor(hovered.t_ms)}
                      cy={yFor(values[hover as number][s.key] ?? 0)}
                      r="4.5"
                      style={{ fill: DB_COLORS[s.role].color, stroke: "hsl(var(--card))" }}
                      strokeWidth="2"
                      vectorEffect="non-scaling-stroke"
                    />
                  ))}
                </g>
              )}
            </svg>
            <div className="pointer-events-none absolute bottom-1 left-[62px] right-[16px] z-10 flex min-w-0 justify-between gap-1 text-[10px] tabular-nums text-muted">
              {xLabels.map((i) => (
                <span key={`label-${i}`} className="min-w-0 max-w-[22%] break-words text-center">{dbUtcShort(buckets[i].t_ms)}</span>
              ))}
            </div>
            {hovered && (
              <div
                id={tooltipId}
                role="tooltip"
                className="pointer-events-none absolute top-2 z-20 w-56 max-w-[calc(100%-1.5rem)] rounded-lg border border-border bg-card p-3 text-xs shadow-card"
                style={{
                  left: `${(xFor(hovered.t_ms) / W) * 100}%`,
                  transform: xFor(hovered.t_ms) / W > 0.7 ? "translateX(-100%)" : xFor(hovered.t_ms) / W < 0.3 ? "translateX(0)" : "translateX(-50%)",
                }}
              >
                <div className="mb-2 min-w-0 break-words font-medium tabular-nums text-fg">{dbUtc(hovered.t_ms)}</div>
                {shown.map((s) => (
                  <div key={`tip-${s.key}`} className="mb-1 flex min-w-0 items-center gap-2">
                    <span className="h-0.5 w-4 shrink-0 rounded-full" style={{ background: DB_COLORS[s.role].color }} aria-hidden="true" />
                    <strong className="shrink-0 tabular-nums text-fg">{panel.format(values[hover as number][s.key] ?? 0)}</strong>
                    <span className="min-w-0 break-words text-secondary">{s.label}</span>
                  </div>
                ))}
                {showAverage && average !== null && (
                  <div className="mt-2 min-w-0 break-words border-t border-border pt-2 text-secondary">
                    Computed average: {panel.format(average)}
                  </div>
                )}
              </div>
            )}
          </div>
          <div className="flex min-w-0 flex-col gap-2 border-t border-border px-4 py-3 sm:flex-row sm:items-center sm:justify-between sm:px-5">
            <p className="min-w-0 break-words text-xs text-secondary">
              Dotted line is the mean of the measured buckets shown. Gaps are not filled.
            </p>
            <Button variant="outline" aria-expanded={tableOpen} aria-controls={tableId} className="min-h-[44px] min-w-0 shrink-0" onClick={() => setTableOpen((v) => !v)}>
              {tableOpen ? "Hide table view" : "Table view"}
            </Button>
          </div>
          {tableOpen && (
            <div id={tableId} className="min-w-0 w-full max-w-full overflow-x-auto border-t border-border">
              <div className="max-h-64 min-w-0 w-full overflow-y-auto">
                <table className="w-full min-w-[420px] table-fixed border-collapse text-xs">
                  <thead>
                    <tr className="border-b border-border text-muted">
                      <th scope="col" className="min-w-0 break-words p-3 text-left">Bucket (UTC)</th>
                      {shown.map((s) => (
                        <th key={`th-${s.key}`} scope="col" className="min-w-0 break-words p-3 text-left">{s.label}</th>
                      ))}
                      {showAverage && average !== null && <th scope="col" className="min-w-0 break-words p-3 text-left">Average</th>}
                    </tr>
                  </thead>
                  <tbody>
                    {buckets.map((b, i) => (
                      <tr key={`row-${b.t_ms}-${i}`} className="border-b border-border align-top">
                        <th scope="row" className="min-w-0 break-words p-3 text-left font-medium tabular-nums text-fg">{dbUtc(b.t_ms)}</th>
                        {shown.map((s) => (
                          <td key={`cell-${b.t_ms}-${s.key}`} className="min-w-0 break-words p-3 tabular-nums">{panel.format(values[i][s.key] ?? 0)}</td>
                        ))}
                        {showAverage && average !== null && <td className="min-w-0 break-words p-3 tabular-nums text-secondary">{panel.format(average)}</td>}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          )}
        </>
      )}
    </Card>
  );
}

/** Small multiples for one database. `series` is a single ASCENDING array of real 60,000 ms
 *  buckets; both windows are slices of that same array, so a narrower view never invents a
 *  bucket. A node on an older build sends no `series` at all: every panel then states the absence. */
export function DbChartGrid({
  data,
  error,
  onRefresh,
}: {
  data: DbInstanceMetrics | null;
  error: string | null;
  onRefresh?: () => void;
}) {
  const [windowKey, setWindowKey] = useState<"60m" | "3h">("60m");
  const { buckets, omitted } = useMemo(() => cleanBuckets(data?.series), [data]);
  const reported = Array.isArray(data?.series);
  const granularity = dbFinite(data?.granularity_ms);
  const windowMinutes = dbFinite(data?.window_minutes);
  const windowed = useMemo(() => {
    const cap = windowKey === "60m" ? 60 : 180;
    return buckets.slice(-cap);
  }, [buckets, windowKey]);
  const diffs: number[] = [];
  for (let i = 1; i < windowed.length; i += 1) diffs.push(windowed[i].t_ms - windowed[i - 1].t_ms);
  diffs.sort((a, b) => a - b);
  const median = diffs.length ? diffs[Math.floor(diffs.length / 2)] : 0;
  /** Gaps are judged against a width the node itself reported, or the median gap between its own
   *  real timestamps. 60,000 ms is only a last resort for a one-bucket series. */
  const stepMs = granularity && granularity > 0 ? granularity : median > 0 ? median : 60_000;

  return (
    <section aria-label="Measured series" className="flex min-w-0 w-full max-w-full flex-col gap-4">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <span className="shrink-0 text-xs font-medium uppercase tracking-wide text-muted">Window</span>
        {(["60m", "3h"] as const).map((key) => {
          const isActive = key === windowKey;
          return (
            <Button
              key={key}
              variant={isActive ? "default" : "outline"}
              aria-pressed={isActive}
              className="min-h-[44px] min-w-0 px-3 text-xs"
              style={isActive
                ? { background: "linear-gradient(90deg, var(--db-indigo-lit, #6366f1), var(--db-cyan-lit, #22d3ee))", color: "#ffffff", borderColor: "transparent" }
                : undefined}
              onClick={() => setWindowKey(key)}
            >
              {key}
            </Button>
          );
        })}
        {onRefresh && (
          <Button variant="outline" className="min-h-[44px] min-w-0 text-xs" onClick={onRefresh}>Refresh</Button>
        )}
        <span className="min-w-0 break-words text-xs tabular-nums text-muted">
          {reported
            ? `${buckets.length} buckets returned${granularity ? ` · ${dbPlain(granularity)} ms each` : ""}${windowMinutes ? ` · ${dbPlain(windowMinutes)} min window` : ""}`
            : "series not reported by this node"}
        </span>
      </div>

      {error && <StateNotice>Couldn&apos;t load metrics: {error.replace(/^Error:\s*/, "")}</StateNotice>}

      {!reported && !error && (
        <Card className="flex min-w-0 items-center gap-3">
          <span
            className="db-fill h-14 w-1.5 shrink-0 rounded-full"
            style={{ background: "linear-gradient(180deg, var(--db-indigo-lit, #6366f1), var(--db-violet-lit, #a78bfa))" }}
            aria-hidden="true"
          />
          <p className="min-w-0 break-words text-sm text-secondary">
            This metrics response has no measured series. Charts appear here once the node reports its 60-second buckets; counters and gauges above remain available.
          </p>
        </Card>
      )}

      {omitted > 0 && reported && (
        <p className="min-w-0 break-words text-xs text-muted">
          {dbPlain(omitted)} returned {omitted === 1 ? "bucket was" : "buckets were"} incomplete and are not plotted; no value was filled in.
        </p>
      )}

      {reported && (
        <div className="grid min-w-0 w-full max-w-full grid-cols-1 gap-4 xl:grid-cols-2">
          {PANELS.map((panel) => (
            <DbChartPanel
              key={panel.id}
              panel={panel}
              buckets={windowed}
              stepMs={stepMs}
              windowLabel={windowKey}
            />
          ))}
        </div>
      )}

      {reported && (
        <p className="min-w-0 break-words text-xs leading-snug text-secondary">
          Every stroke passes through a bucket the node measured. Curves, areas and the dotted average are computed from those buckets alone; a missing bucket stays a gap. Times are UTC as reported by the server.
        </p>
      )}
    </section>
  );
}
