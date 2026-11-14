"use client";

import { useId, useState, type KeyboardEvent, type PointerEvent } from "react";
import { Button, Card } from "@/components/ui";
import {
  ANALYTICS_CARD_CLASS,
  ANALYTICS_MARK,
  ANALYTICS_PILL_CLASS,
  ANALYTICS_TAB_CLASS,
  ANALYTICS_TEXT,
  ANALYTICS_TOKENS,
  analyticsSeriesDash,
} from "@/components/analytics-theme";

export interface ChartSeries {
  key: string;
  label: string;
  color: string;
  lit: string;
  format: (value: number) => string;
}

export interface ChartColumn {
  t_ms: number;
  label: string;
  values: Record<string, number>;
  projected?: boolean;
  measuredValues?: Record<string, number>;
}

const W = 1000;
const H = 320;
const LEFT = 52;
const RIGHT = 18;
const TOP = 20;
const BOTTOM = 36;
const PLOT_W = W - LEFT - RIGHT;
const PLOT_H = H - TOP - BOTTOM;
const TICKS = [100, 75, 50, 25, 0];

function finite(value: number | null | undefined): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

export function AnalyticsChart({
  columns,
  series,
  activeKeys,
  onToggle,
  solidEndIndex,
  projectionIndex,
  windowLabel,
}: {
  columns: ChartColumn[];
  series: ChartSeries[];
  activeKeys: string[];
  onToggle: (key: string) => void;
  solidEndIndex: number;
  projectionIndex: number | null;
  windowLabel: string;
}) {
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const tooltipId = uid + "-tooltip";
  const tableId = uid + "-table";
  const [hover, setHover] = useState<number | null>(null);
  const [tableOpen, setTableOpen] = useState(false);
  const active = series.filter((item) => activeKeys.includes(item.key));
  const hasColumns = columns.length > 0 && active.length > 0;
  const minT = columns.length ? columns[0].t_ms : 0;
  const maxT = columns.length ? columns[columns.length - 1].t_ms : 1;
  const span = Math.max(1, maxT - minT);
  const xFor = (t: number) => LEFT + ((t - minT) / span) * PLOT_W;
  const yFor = (pct: number) => TOP + (1 - pct / 100) * PLOT_H;
  const peaks: Record<string, number> = {};
  for (const item of active) {
    let peak = 0;
    for (const column of columns) {
      const value = finite(column.values[item.key]);
      if (value > peak) peak = value;
    }
    peaks[item.key] = peak;
  }
  const norm = (column: ChartColumn, key: string) => peaks[key] > 0 ? (finite(column.values[key]) / peaks[key]) * 100 : 0;
  const pathFor = (indices: number[], key: string) => indices.map((index, position) => {
    const column = columns[index];
    return (position === 0 ? "M " : "L ") + xFor(column.t_ms).toFixed(2) + " " + yFor(norm(column, key)).toFixed(2);
  }).join(" ");
  const solidIdxs: number[] = [];
  if (hasColumns) for (let index = 0; index <= solidEndIndex && index < columns.length; index += 1) solidIdxs.push(index);
  const dashedIdxs: number[] = [];
  if (projectionIndex !== null && projectionIndex > solidEndIndex && projectionIndex < columns.length) {
    for (let index = Math.max(0, solidEndIndex); index <= projectionIndex; index += 1) dashedIdxs.push(index);
  }
  const lastSolid = solidIdxs.length ? solidIdxs[solidIdxs.length - 1] : 0;
  const partialIndex = projectionIndex !== null ? projectionIndex - 1 : -1;
  const labelCandidates = projectionIndex !== null
    ? [0, Math.floor(lastSolid / 2), lastSolid, projectionIndex]
    : [0, Math.floor((columns.length - 1) / 2), columns.length - 1];
  const xLabelIdxs = Array.from(new Set(labelCandidates.filter((index) => index >= 0 && index < columns.length)));
  const hoverColumn = hover !== null && hover >= 0 && hover < columns.length ? columns[hover] : null;

  function nearestIndex(clientX: number, rect: DOMRect): number {
    if (!columns.length || rect.width <= 0) return 0;
    const fraction = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    const viewX = fraction * W;
    const targetT = minT + ((viewX - LEFT) / PLOT_W) * span;
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

  return (
    <Card className={ANALYTICS_CARD_CLASS} aria-label={windowLabel + " measured series"}>
      <div className="flex min-w-0 flex-col gap-3 border-b border-border p-4 sm:p-5 lg:flex-row lg:items-center lg:justify-between">
        <div className="min-w-0">
          <h2 className={ANALYTICS_TEXT.title}>Measured series</h2>
          <p className={"mt-1 " + ANALYTICS_TEXT.caption}>Percent of each series measured peak</p>
        </div>
        <div aria-label="Series legend" className="flex min-w-0 flex-wrap gap-2">
          {series.map((item, seriesIndex) => {
            const isActive = activeKeys.includes(item.key);
            return (
              <Button
                key={item.key}
                variant="ghost"
                aria-pressed={isActive}
                className={ANALYTICS_TAB_CLASS + (isActive ? " gap-2 font-medium text-fg" : " gap-2 text-secondary")}
                style={isActive ? { borderBottomColor: ANALYTICS_TOKENS.underline } : undefined}
                onClick={() => onToggle(item.key)}
              >
                <svg width="20" height="8" viewBox="0 0 20 8" aria-hidden="true">
                  <line
                    x1="0"
                    y1="4"
                    x2="20"
                    y2="4"
                    stroke={ANALYTICS_TOKENS.accent}
                    strokeWidth={ANALYTICS_MARK.lineWidth}
                    strokeDasharray={analyticsSeriesDash(seriesIndex)}
                  />
                </svg>
                <span className="min-w-0 break-words">{item.label}</span>
              </Button>
            );
          })}
        </div>
      </div>
      {!hasColumns ? (
        <div className="flex min-h-[220px] min-w-0 items-center gap-3 p-5">
          <span className="h-12 w-px shrink-0 bg-border" aria-hidden="true" />
          <p className={ANALYTICS_TEXT.caption}>
            {columns.length === 0 ? "No measured buckets are returned for this window." : "Select at least one metric."}
          </p>
        </div>
      ) : (
        <>
          <div
            className="relative h-[300px] min-w-0 w-full max-w-full overflow-hidden bg-transparent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-border sm:h-[360px]"
            role="group"
            tabIndex={0}
            aria-label={windowLabel + " chart. Use arrow keys to inspect measured buckets."}
            aria-describedby={hoverColumn ? tooltipId : undefined}
            style={{ touchAction: "pan-y" }}
            onPointerMove={(event: PointerEvent<HTMLDivElement>) => setHover(nearestIndex(event.clientX, event.currentTarget.getBoundingClientRect()))}
            onPointerLeave={() => setHover(null)}
            onFocus={() => setHover(columns.length - 1)}
            onBlur={() => setHover(null)}
            onKeyDown={onChartKeyDown}
          >
            <div className={"pointer-events-none absolute left-2 top-[18px] bottom-[38px] z-10 flex flex-col justify-between " + ANALYTICS_TEXT.axis}>
              {TICKS.map((tick) => <span key={tick}>{tick + "%"}</span>)}
            </div>
            <svg viewBox="0 0 1000 320" preserveAspectRatio="none" className="absolute inset-0 h-full w-full" aria-hidden="true">
              {TICKS.map((tick) => (
                <line
                  key={"y-" + tick}
                  x1={LEFT}
                  x2={W - RIGHT}
                  y1={yFor(tick)}
                  y2={yFor(tick)}
                  stroke={ANALYTICS_TOKENS.grid}
                  strokeWidth={ANALYTICS_MARK.gridWidth}
                  vectorEffect="non-scaling-stroke"
                />
              ))}
              {active.map((item, seriesIndex) => {
                const line = pathFor(solidIdxs, item.key);
                const area = solidIdxs.length > 1
                  ? line + " L " + xFor(columns[lastSolid].t_ms).toFixed(2) + " " + yFor(0) + " L " + xFor(columns[solidIdxs[0]].t_ms).toFixed(2) + " " + yFor(0) + " Z"
                  : "";
                return (
                  <g key={"area-" + item.key}>
                    {area ? <path d={area} fill={ANALYTICS_TOKENS.accentSoft} /> : null}
                    <path
                      d={line}
                      fill="none"
                      stroke={ANALYTICS_TOKENS.accent}
                      strokeWidth={ANALYTICS_MARK.lineWidth}
                      strokeDasharray={analyticsSeriesDash(seriesIndex)}
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      vectorEffect="non-scaling-stroke"
                    />
                  </g>
                );
              })}
              {active.map((item, seriesIndex) => {
                const line = dashedIdxs.length > 1 ? pathFor(dashedIdxs, item.key) : "";
                return line
                  ? <path
                      key={"projection-" + item.key}
                      d={line}
                      fill="none"
                      stroke={ANALYTICS_TOKENS.accent}
                      strokeWidth={ANALYTICS_MARK.lineWidth}
                      strokeDasharray={ANALYTICS_MARK.tailDash}
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      vectorEffect="non-scaling-stroke"
                    />
                  : null;
              })}
              {partialIndex >= 0 && partialIndex < columns.length && active.map((item) => (
                <circle
                  key={"partial-" + item.key}
                  cx={xFor(columns[partialIndex].t_ms)}
                  cy={yFor(norm(columns[partialIndex], item.key))}
                  r={ANALYTICS_MARK.markerRadius}
                  fill={ANALYTICS_TOKENS.accent}
                  stroke={ANALYTICS_TOKENS.surface}
                  strokeWidth={ANALYTICS_MARK.lineWidth}
                  vectorEffect="non-scaling-stroke"
                />
              ))}
              {projectionIndex !== null && active.map((item) => (
                <circle
                  key={"projected-" + item.key}
                  cx={xFor(columns[projectionIndex].t_ms)}
                  cy={yFor(norm(columns[projectionIndex], item.key))}
                  r={ANALYTICS_MARK.markerRadius}
                  fill={ANALYTICS_TOKENS.surface}
                  stroke={ANALYTICS_TOKENS.accent}
                  strokeWidth={ANALYTICS_MARK.lineWidth}
                  vectorEffect="non-scaling-stroke"
                />
              ))}
              {active.map((item, index) => {
                const column = columns[lastSolid];
                const labelX = Math.min(xFor(column.t_ms) + 10, W - RIGHT - 150);
                const labelY = Math.max(TOP + 10, Math.min(H - BOTTOM - 8, yFor(norm(column, item.key)) + (index - (active.length - 1) / 2) * 14));
                return (
                  <g key={"label-" + item.key} transform={"translate(" + labelX.toFixed(2) + "," + labelY.toFixed(2) + ")"}>
                    <line x1="0" y1="0" x2="16" y2="0" stroke={ANALYTICS_TOKENS.accent} strokeWidth={ANALYTICS_MARK.lineWidth} vectorEffect="non-scaling-stroke" />
                    <text x="20" y="4" fontSize="11" fill={ANALYTICS_TOKENS.text}>{item.label}</text>
                  </g>
                );
              })}
              {hover !== null && hoverColumn ? (
                <g>
                  <line
                    x1={xFor(hoverColumn.t_ms)}
                    x2={xFor(hoverColumn.t_ms)}
                    y1={TOP}
                    y2={H - BOTTOM}
                    stroke={ANALYTICS_TOKENS.neutralLine}
                    strokeWidth={ANALYTICS_MARK.gridWidth}
                    strokeDasharray={ANALYTICS_MARK.crosshairDash}
                    vectorEffect="non-scaling-stroke"
                  />
                  {active.map((item) => (
                    <circle
                      key={"hover-" + item.key}
                      cx={xFor(hoverColumn.t_ms)}
                      cy={yFor(norm(hoverColumn, item.key))}
                      r={ANALYTICS_MARK.markerRadius}
                      fill={ANALYTICS_TOKENS.accent}
                      stroke={ANALYTICS_TOKENS.surface}
                      strokeWidth={ANALYTICS_MARK.lineWidth}
                      vectorEffect="non-scaling-stroke"
                    />
                  ))}
                </g>
              ) : null}
            </svg>
            <div className={"pointer-events-none absolute bottom-2 left-[52px] right-[18px] z-10 flex min-w-0 justify-between gap-1 " + ANALYTICS_TEXT.axis}>
              {xLabelIdxs.map((index) => <span key={"axis-" + index} className="min-w-0 max-w-[24%] break-words text-center">{columns[index].label}</span>)}
            </div>
            {hover !== null && hoverColumn ? (
              <div
                id={tooltipId}
                role="tooltip"
                className="pointer-events-none absolute top-3 z-20 w-56 max-w-[calc(100%-1.5rem)] rounded-md border border-border bg-card p-3 text-xs shadow-none"
                style={{
                  left: (xFor(hoverColumn.t_ms) / W) * 100 + "%",
                  transform: xFor(hoverColumn.t_ms) / W > 0.7 ? "translateX(-100%)" : xFor(hoverColumn.t_ms) / W < 0.3 ? "translateX(0)" : "translateX(-50%)",
                }}
              >
                <div className="mb-2 min-w-0 break-words font-medium text-fg">
                  {hoverColumn.label}{hoverColumn.projected ? " · projected" : ""}
                </div>
                {active.map((item, seriesIndex) => (
                  <div key={"tip-" + item.key} className="mb-1 flex min-w-0 items-center gap-2">
                    <svg width="20" height="8" viewBox="0 0 20 8" aria-hidden="true">
                      <line x1="0" y1="4" x2="20" y2="4" stroke={ANALYTICS_TOKENS.accent} strokeWidth={ANALYTICS_MARK.lineWidth} strokeDasharray={analyticsSeriesDash(seriesIndex)} />
                    </svg>
                    <strong className="shrink-0 tabular-nums text-fg">{item.format(finite(hoverColumn.values[item.key]))}</strong>
                    <span className="min-w-0 break-words text-secondary">{item.label}</span>
                  </div>
                ))}
                {hoverColumn.projected && hoverColumn.measuredValues ? (
                  <div className="mt-2 min-w-0 break-words border-t border-border pt-2 text-secondary">
                    {"Measured so far: " + active.map((item) => item.label + " " + item.format(finite(hoverColumn.measuredValues?.[item.key]))).join(" · ")}
                  </div>
                ) : null}
              </div>
            ) : null}
          </div>
          <div className="flex min-w-0 flex-col gap-2 border-t border-border p-4 sm:flex-row sm:items-center sm:justify-between sm:p-5">
            <p className={ANALYTICS_TEXT.caption}>
              {projectionIndex !== null
                ? "Dashed segments project the current bucket from its measured rate."
                : "No partial bucket is available for projection."}
            </p>
            <Button
              variant="outline"
              aria-expanded={tableOpen}
              aria-controls={tableId}
              className={ANALYTICS_PILL_CLASS}
              onClick={() => setTableOpen((value) => !value)}
            >
              {tableOpen ? "Hide table view" : "Table view"}
            </Button>
          </div>
          {tableOpen ? (
            <div id={tableId} className="max-h-64 min-w-0 w-full max-w-full overflow-y-auto border-t border-border">
              <table className="w-full max-w-full table-fixed border-collapse text-xs">
                <colgroup><col className="w-[40%]" /><col className="w-[60%]" /></colgroup>
                <thead>
                  <tr className="border-b border-border text-muted">
                    <th scope="col" className="min-w-0 break-words p-3 text-left">Bucket</th>
                    <th scope="col" className="min-w-0 break-words p-3 text-left">Measurements</th>
                  </tr>
                </thead>
                <tbody>
                  {columns.map((column, index) => (
                    <tr key={"row-" + column.t_ms + "-" + index} className="border-b border-border align-top">
                      <th scope="row" className="min-w-0 break-words p-3 text-left font-medium text-fg">
                        {column.label}{column.projected ? <span className="ml-1 text-muted">(projected)</span> : null}
                      </th>
                      <td className="min-w-0 break-words p-3">
                        <div className="grid min-w-0 grid-cols-1 gap-1 sm:grid-cols-2">
                          {active.map((item, seriesIndex) => (
                            <span key={"cell-" + column.t_ms + "-" + item.key} className="flex min-w-0 items-center gap-1.5">
                              <svg width="14" height="8" viewBox="0 0 14 8" aria-hidden="true">
                                <line x1="0" y1="4" x2="14" y2="4" stroke={ANALYTICS_TOKENS.accent} strokeWidth={ANALYTICS_MARK.lineWidth} strokeDasharray={analyticsSeriesDash(seriesIndex)} />
                              </svg>
                              <strong className="shrink-0 tabular-nums text-fg">{item.format(finite(column.values[item.key]))}</strong>
                              <span className="min-w-0 break-words text-secondary">{item.label}</span>
                            </span>
                          ))}
                          {column.projected && column.measuredValues ? (
                            <span className="col-span-full min-w-0 break-words text-secondary">
                              {"Measured so far: " + active.map((item) => item.label + " " + item.format(finite(column.measuredValues?.[item.key]))).join(" · ")}
                            </span>
                          ) : null}
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : null}
        </>
      )}
    </Card>
  );
}
