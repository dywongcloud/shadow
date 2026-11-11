"use client";

import { useId, useState, type KeyboardEvent, type PointerEvent } from "react";
import { Button, Card } from "@/components/ui";

export interface StorageChartSeries {
  key: string;
  label: string;
  color: string;
}

/** `values` carries ONLY readings the node reported. `null` = that database did not report this
 *  bucket, so the line breaks there instead of being joined through an invented zero. */
export interface StorageChartColumn {
  t_ms: number;
  label: string;
  values: Record<string, number | null>;
}

export interface ChartPoint {
  x: number;
  y: number;
}

export interface ChartSegment {
  start: number;
  end: number;
  points: ChartPoint[];
}

/** Contiguous runs of real values. One real bucket still draws a marker. */
export function chartSegments(
  values: (number | null)[],
  xFor: (index: number) => number,
  yFor: (value: number) => number,
): ChartSegment[] {
  const segments: ChartSegment[] = [];
  let open = false;
  for (let index = 0; index < values.length; index += 1) {
    const value = values[index];
    if (value === null || value === undefined || !Number.isFinite(value)) {
      open = false;
      continue;
    }
    const point = { x: xFor(index), y: yFor(value) };
    if (!open) {
      segments.push({ start: index, end: index, points: [point] });
      open = true;
    } else {
      const segment = segments[segments.length - 1];
      segment.points.push(point);
      segment.end = index;
    }
  }
  return segments;
}

export function segmentPath(points: ChartPoint[]): string {
  return points
    .map((point, index) => (index === 0 ? "M " : "L ") + point.x.toFixed(2) + " " + point.y.toFixed(2))
    .join(" ");
}

const W = 1000;
const H = 320;
const LEFT = 56;
const RIGHT = 18;
const TOP = 18;
const BOTTOM = 34;
const PLOT_W = W - LEFT - RIGHT;
const PLOT_H = H - TOP - BOTTOM;
const TICKS = [1, 0.75, 0.5, 0.25, 0];
const BASELINE = TOP + PLOT_H;

function finite(value: number | null | undefined): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/** One multi-series measured line chart: coloured strokes over per-series gradient fills, a
 *  computed dotted Average, a pointer/keyboard crosshair, and a table view of the same columns. */
export function StorageAnalyticsChart({
  title,
  caption,
  columns,
  series,
  activeKeys,
  average,
  format,
  windowLabel,
  emptyText,
  showTotal = false,
}: {
  title: string;
  caption: string;
  columns: StorageChartColumn[];
  series: StorageChartSeries[];
  activeKeys: string[];
  average: (number | null)[];
  format: (value: number) => string;
  windowLabel: string;
  emptyText: string;
  showTotal?: boolean;
}) {
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const tooltipId = uid + "-storage-tooltip";
  const tableId = uid + "-storage-table";
  const [hover, setHover] = useState<number | null>(null);
  const [tableOpen, setTableOpen] = useState(false);
  const active = series.filter((item) => activeKeys.includes(item.key));
  const firstT = columns.length ? columns[0].t_ms : 0;
  const lastT = columns.length ? columns[columns.length - 1].t_ms : firstT;
  const span = Math.max(1, lastT - firstT);
  const xFor = (index: number) =>
    columns.length === 1
      ? LEFT + PLOT_W / 2
      : LEFT + ((columns[index].t_ms - firstT) / span) * PLOT_W;

  let peak = 0;
  for (let index = 0; index < columns.length; index += 1) {
    for (const item of active) {
      const value = finite(columns[index].values[item.key]);
      if (value !== null && value > peak) peak = value;
    }
    const averageValue = finite(average[index]);
    if (averageValue !== null && averageValue > peak) peak = averageValue;
  }
  const max = peak > 0 ? peak : 1;
  const yFor = (value: number) => TOP + (1 - value / max) * PLOT_H;
  const hasValues = columns.some((column, index) =>
    active.some((item) => finite(column.values[item.key]) !== null) || finite(average[index]) !== null,
  );
  const labelIndices = Array.from(
    new Set(
      columns.length
        ? [0, Math.floor((columns.length - 1) / 3), Math.floor((2 * (columns.length - 1)) / 3), columns.length - 1]
        : [],
    ),
  );
  const gradientId = (key: string) => uid + "-storage-fill-" + key.replace(/[^a-zA-Z0-9_-]/g, "");
  const hoverColumn = hover !== null && hover >= 0 && hover < columns.length ? columns[hover] : null;
  const hoverAverage = hover !== null ? finite(average[hover]) : null;
  const hoverMissing = hoverColumn
    ? active.filter((item) => finite(hoverColumn.values[item.key]) === null).length
    : 0;
  const hoverTotal = hoverColumn
    ? (() => {
        let sum = 0;
        let count = 0;
        for (const item of active) {
          const value = finite(hoverColumn.values[item.key]);
          if (value !== null) {
            sum += value;
            count += 1;
          }
        }
        return count > 0 ? sum : null;
      })()
    : null;
  const hoverRight = hoverColumn ? xFor(hover ?? 0) / W > 0.55 : false;
  const legend = active.slice(0, 6);

  function totalFor(column: StorageChartColumn): number | null {
    let sum = 0;
    let count = 0;
    for (const item of active) {
      const value = finite(column.values[item.key]);
      if (value !== null) {
        sum += value;
        count += 1;
      }
    }
    return count > 0 ? sum : null;
  }

  function nearestIndex(clientX: number, rect: DOMRect): number {
    if (!columns.length || rect.width <= 0) return 0;
    const fraction = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    const targetT = firstT + ((fraction * W - LEFT) / PLOT_W) * span;
    let best = 0;
    let bestDistance = Infinity;
    for (let index = 0; index < columns.length; index += 1) {
      const distance = Math.abs(columns[index].t_ms - targetT);
      if (distance < bestDistance) {
        bestDistance = distance;
        best = index;
      }
    }
    return best;
  }

  function onChartKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (!columns.length) return;
    if (event.key === "ArrowLeft" || event.key === "ArrowRight" || event.key === "Home" || event.key === "End") {
      event.preventDefault();
      const base = hover ?? columns.length - 1;
      if (event.key === "Home") setHover(0);
      else if (event.key === "End") setHover(columns.length - 1);
      else if (event.key === "ArrowLeft") setHover(Math.max(0, base - 1));
      else setHover(Math.min(columns.length - 1, base + 1));
    }
  }

  function draw(values: (number | null)[], color: string, gradientKey: string, dotted: boolean) {
    const segments = chartSegments(values, xFor, yFor);
    return segments.map((segment) => {
      const points = segment.points;
      if (points.length === 1) {
        return (
          <circle
            key={segment.start}
            cx={points[0].x}
            cy={points[0].y}
            r="4"
            style={{ fill: color, stroke: "hsl(var(--card))" }}
            strokeWidth="2"
            vectorEffect="non-scaling-stroke"
          />
        );
      }
      const line = segmentPath(points);
      const first = points[0];
      const last = points[points.length - 1];
      const area = line + " L " + last.x.toFixed(2) + " " + BASELINE + " L " + first.x.toFixed(2) + " " + BASELINE + " Z";
      return (
        <g key={segment.start}>
          {dotted ? null : <path d={area} fill={"url(#" + gradientId(gradientKey) + ")"} />}
          {dotted ? null : (
            <path
              d={line}
              fill="none"
              style={{ stroke: color }}
              strokeOpacity="0.14"
              strokeWidth="8"
              strokeLinecap="round"
              strokeLinejoin="round"
              vectorEffect="non-scaling-stroke"
            />
          )}
          <path
            d={line}
            fill="none"
            style={{ stroke: color }}
            strokeOpacity={dotted ? 0.9 : 1}
            strokeWidth={dotted ? 2 : 2.25}
            strokeDasharray={dotted ? "3 6" : undefined}
            strokeLinecap="round"
            strokeLinejoin="round"
            vectorEffect="non-scaling-stroke"
          />
        </g>
      );
    });
  }

  return (
    <Card className="min-w-0 w-full max-w-full p-0" aria-label={title}>
      <div className="flex min-w-0 flex-col gap-2 border-b border-border p-4 sm:p-5 lg:flex-row lg:items-start lg:justify-between">
        <div className="min-w-0">
          <h2 className="min-w-0 break-words text-sm font-semibold text-fg">{title}</h2>
          <p className="mt-1 min-w-0 break-words text-xs leading-snug text-secondary">{caption}</p>
        </div>
        <div className="flex min-w-0 flex-wrap items-center gap-2" aria-label="Chart legend">
          {legend.map((item) => (
            <span key={item.key} className="inline-flex min-w-0 max-w-full items-center gap-1.5 text-xs text-secondary">
              <span className="h-0.5 w-4 shrink-0 rounded-full" style={{ background: item.color }} aria-hidden="true" />
              <span className="min-w-0 break-words">{item.label}</span>
            </span>
          ))}
          {active.length > legend.length ? (
            <span className="shrink-0 text-xs text-muted">+{active.length - legend.length} more</span>
          ) : null}
          <span className="inline-flex shrink-0 items-center gap-1.5 text-xs text-secondary">
            <svg width="20" height="8" viewBox="0 0 20 8" aria-hidden="true">
              <line x1="0" y1="4" x2="20" y2="4" stroke="hsl(var(--fg))" strokeWidth="2" strokeDasharray="3 4" />
            </svg>
            Average
          </span>
        </div>
      </div>

      {columns.length === 0 || active.length === 0 || !hasValues ? (
        <div className="flex min-h-[220px] min-w-0 items-center gap-3 p-5">
          <span
            className="h-16 w-1.5 shrink-0 rounded-full"
            style={{ background: "linear-gradient(180deg, #6366f1, #22d3ee)" }}
            aria-hidden="true"
          />
          <p className="min-w-0 break-words text-sm text-secondary">
            {columns.length === 0
              ? emptyText
              : active.length === 0
                ? "Select at least one database."
                : emptyText}
          </p>
        </div>
      ) : (
        <>
          <div
            className="relative h-[300px] min-w-0 w-full max-w-full overflow-hidden rounded-lg border border-border bg-card sm:h-[360px]"
            role="group"
            tabIndex={0}
            aria-label={windowLabel + " " + title + ". Use arrow keys to inspect reported buckets."}
            aria-describedby={hoverColumn ? tooltipId : undefined}
            style={{ touchAction: "pan-y" }}
            onPointerMove={(event: PointerEvent<HTMLDivElement>) =>
              setHover(nearestIndex(event.clientX, event.currentTarget.getBoundingClientRect()))
            }
            onPointerLeave={() => setHover(null)}
            onFocus={() => setHover(columns.length - 1)}
            onBlur={() => setHover(null)}
            onKeyDown={onChartKeyDown}
          >
            <div className="pointer-events-none absolute left-1 z-10 flex w-[48px] flex-col justify-between text-right text-[10px] tabular-nums text-muted" style={{ top: "5.625%", bottom: "10.625%" }}>
              {TICKS.map((tick) => (
                <span key={tick} className="min-w-0 break-words">{format(max * tick)}</span>
              ))}
            </div>
            <svg viewBox="0 0 1000 320" preserveAspectRatio="none" className="absolute inset-0 h-full w-full" aria-hidden="true">
              <defs>
                {active.map((item) => (
                  <linearGradient key={item.key} id={gradientId(item.key)} x1="0" y1="0" x2="0" y2="1">
                    <stop offset="0%" style={{ stopColor: item.color, stopOpacity: 0.32 }} />
                    <stop offset="100%" style={{ stopColor: item.color, stopOpacity: 0.01 }} />
                  </linearGradient>
                ))}
              </defs>
              {TICKS.map((tick) => (
                <line
                  key={"y-" + tick}
                  x1={LEFT}
                  x2={W - RIGHT}
                  y1={yFor(max * tick)}
                  y2={yFor(max * tick)}
                  stroke="hsl(var(--border))"
                  strokeWidth="1"
                  strokeDasharray={tick === 0 ? undefined : "2 7"}
                  vectorEffect="non-scaling-stroke"
                />
              ))}
              {labelIndices.map((index) => (
                <line
                  key={"x-" + index}
                  x1={xFor(index)}
                  x2={xFor(index)}
                  y1={TOP}
                  y2={BASELINE}
                  stroke="hsl(var(--border))"
                  strokeWidth="1"
                  strokeDasharray="2 9"
                  vectorEffect="non-scaling-stroke"
                />
              ))}
              {active.map((item) => (
                <g key={item.key}>
                  {draw(columns.map((column) => finite(column.values[item.key])), item.color, item.key, false)}
                </g>
              ))}
              <g>{draw(average, "hsl(var(--fg))", "average", true)}</g>
              {hover !== null && hoverColumn ? (
                <g>
                  <line
                    x1={xFor(hover)}
                    x2={xFor(hover)}
                    y1={TOP}
                    y2={BASELINE}
                    stroke="hsl(var(--border-strong))"
                    strokeWidth="1"
                    strokeDasharray="3 4"
                    vectorEffect="non-scaling-stroke"
                  />
                  {active.map((item) => {
                    const value = finite(hoverColumn.values[item.key]);
                    return value === null ? null : (
                      <circle
                        key={"hover-" + item.key}
                        cx={xFor(hover)}
                        cy={yFor(value)}
                        r="4"
                        style={{ fill: item.color, stroke: "hsl(var(--card))" }}
                        strokeWidth="2"
                        vectorEffect="non-scaling-stroke"
                      />
                    );
                  })}
                  {hoverAverage === null ? null : (
                    <circle
                      cx={xFor(hover)}
                      cy={yFor(hoverAverage)}
                      r="4"
                      style={{ fill: "hsl(var(--card))", stroke: "hsl(var(--fg))" }}
                      strokeWidth="2"
                      vectorEffect="non-scaling-stroke"
                    />
                  )}
                </g>
              ) : null}
            </svg>
            <div className="pointer-events-none absolute bottom-2 z-10 flex min-w-0 justify-between gap-1 text-[10px] text-muted" style={{ left: "5.6%", right: "1.8%" }}>
              {labelIndices.map((index) => (
                <span key={"axis-" + index} className="min-w-0 max-w-[24%] break-words text-center">
                  {columns[index].label}
                </span>
              ))}
            </div>
            {hover !== null && hoverColumn ? (
              <div
                id={tooltipId}
                role="tooltip"
                className="pointer-events-none absolute top-3 z-20 w-56 max-w-[calc(100%-1.5rem)] rounded-lg border border-border bg-card p-3 text-xs shadow-card"
                style={hoverRight ? { right: "0.75rem" } : { left: "0.75rem" }}
              >
                <div className="mb-2 min-w-0 break-words font-medium text-fg">
                  {hoverColumn.label} UTC · {windowLabel}
                </div>
                {active.map((item) => {
                  const value = finite(hoverColumn.values[item.key]);
                  return (
                    <div key={"tip-" + item.key} className="mb-1 flex min-w-0 items-center gap-2">
                      <span className="h-0.5 w-4 shrink-0 rounded-full" style={{ background: item.color }} aria-hidden="true" />
                      <strong className="shrink-0 tabular-nums text-fg">{value === null ? "—" : format(value)}</strong>
                      <span className="min-w-0 break-words text-secondary">{item.label}</span>
                    </div>
                  );
                })}
                <div className="mt-2 min-w-0 break-words border-t border-border pt-2 text-secondary">
                  {showTotal ? "Total " + (hoverTotal === null ? "—" : format(hoverTotal)) + " · " : ""}
                  Average {hoverAverage === null ? "—" : format(hoverAverage)}
                </div>
                {hoverMissing > 0 ? (
                  <div className="mt-1 min-w-0 break-words text-muted">
                    {hoverMissing} selected {hoverMissing === 1 ? "database did" : "databases did"} not report this bucket.
                  </div>
                ) : null}
              </div>
            ) : null}
          </div>
          <div className="flex min-w-0 flex-col gap-2 border-t border-border p-4 sm:flex-row sm:items-center sm:justify-between sm:p-5">
            <p className="min-w-0 break-words text-xs text-secondary">
              The dotted Average is the mean of the selected databases that reported each bucket. A gap is an unreported bucket, not a zero.
            </p>
            <Button
              variant="outline"
              aria-expanded={tableOpen}
              aria-controls={tableId}
              className="min-h-[44px] min-w-0 shrink-0"
              onClick={() => setTableOpen((value) => !value)}
            >
              {tableOpen ? "Hide table view" : "Table view"}
            </Button>
          </div>
          {tableOpen ? (
            <div id={tableId} className="max-h-80 min-w-0 w-full max-w-full overflow-auto border-t border-border">
              <table className="w-full min-w-[640px] border-collapse text-xs">
                <thead className="sticky top-0 bg-card">
                  <tr className="border-b border-border text-muted">
                    <th scope="col" className="min-w-0 break-words p-3 text-left">Bucket (UTC)</th>
                    {active.map((item) => (
                      <th key={item.key} scope="col" className="min-w-0 break-words p-3 text-left">
                        <span className="inline-block h-0.5 w-3 shrink-0 rounded-full align-middle" style={{ background: item.color }} aria-hidden="true" />
                        <span className="ml-1.5 min-w-0 break-words">{item.label}</span>
                      </th>
                    ))}
                    {showTotal ? <th scope="col" className="min-w-0 break-words p-3 text-left">Total</th> : null}
                    <th scope="col" className="min-w-0 break-words p-3 text-left">Average</th>
                  </tr>
                </thead>
                <tbody>
                  {columns.map((column, columnIndex) => {
                    const total = totalFor(column);
                    const averageValue = finite(average[columnIndex]);
                    return (
                      <tr key={"row-" + column.t_ms} className="border-b border-border align-top">
                        <th scope="row" className="min-w-0 break-words p-3 text-left font-medium text-fg">{column.label}</th>
                        {active.map((item) => {
                          const value = finite(column.values[item.key]);
                          return (
                            <td key={item.key} className="min-w-0 break-words p-3 tabular-nums">
                              {value === null ? "—" : format(value)}
                            </td>
                          );
                        })}
                        {showTotal ? <td className="min-w-0 break-words p-3 tabular-nums">{total === null ? "—" : format(total)}</td> : null}
                        <td className="min-w-0 break-words p-3 tabular-nums">
                          {averageValue === null ? "—" : format(averageValue)}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          ) : null}
        </>
      )}
    </Card>
  );
}
