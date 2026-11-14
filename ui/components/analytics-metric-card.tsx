"use client";

import { Badge, Card } from "@/components/ui";
import {
  ANALYTICS_CARD_CLASS,
  ANALYTICS_MARK,
  ANALYTICS_TEXT,
  ANALYTICS_TOKENS,
} from "@/components/analytics-theme";

export type MetricDeltaTone = "default" | "green" | "red" | "amber" | "purple";

const W = 240;
const H = 64;
const PAD = 4;

function finite(value: number | null | undefined): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function pathFor(points: { x: number; y: number }[]): string {
  return points
    .map((point, index) => (index === 0 ? "M " : "L ") + point.x.toFixed(2) + " " + point.y.toFixed(2))
    .join(" ");
}

export function AnalyticsMetricCard({
  label,
  value,
  values,
  delta,
  deltaTone,
  deltaHint,
  deltaAria,
  selected = false,
  onSelect,
}: {
  label: string;
  value: string;
  values: number[];
  delta: string;
  deltaTone: MetricDeltaTone;
  deltaHint: string;
  deltaAria: string;
  selected?: boolean;
  onSelect?: () => void;
}) {
  const clean = values.map(finite);
  let max = 0;
  for (const item of clean) if (item > max) max = item;
  const step = clean.length > 1 ? (W - 2 * PAD) / (clean.length - 1) : 0;
  const yFor = (item: number) => H - PAD - (max > 0 ? (item / max) * (H - 2 * PAD) : 0);
  const points = clean.map((item, index) => ({ x: PAD + step * index, y: yFor(item) }));
  const flat = points.length ? points : [{ x: PAD, y: H - PAD }];
  const line = pathFor(flat);
  const area = points.length > 1
    ? line + " L " + points[points.length - 1].x.toFixed(2) + " " + (H - PAD) + " L " + points[0].x.toFixed(2) + " " + (H - PAD) + " Z"
    : "";
  const solidLine = pathFor(points.length > 2 ? points.slice(0, -1) : points.slice(0, 1));
  const tailLine = points.length > 1 ? pathFor(points.slice(-2)) : "";

  return (
    <Card className={ANALYTICS_CARD_CLASS + " relative overflow-hidden"}>
      <button
        type="button"
        role="tab"
        aria-selected={selected}
        aria-label={"Show " + label + " in measured series"}
        onClick={onSelect}
        className="relative block min-w-0 w-full max-w-full text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-border"
      >
        <div className="min-w-0 p-4 sm:p-5">
          <div className="flex min-w-0 items-start justify-between gap-2">
            <div className="min-w-0">
              <div className={ANALYTICS_TEXT.label}>{label}</div>
              <div className={"mt-1 " + ANALYTICS_TEXT.value}>{value}</div>
            </div>
            <Badge tone={deltaTone} className="shrink-0 tabular-nums" aria-label={deltaAria}>{delta}</Badge>
          </div>
          <svg
            viewBox="0 0 240 64"
            preserveAspectRatio="none"
            className="mt-3 h-14 w-full max-w-full"
            role="img"
            aria-label={label + " across " + clean.length + " measured buckets"}
          >
            <line x1={PAD} y1={H - PAD} x2={W - PAD} y2={H - PAD} stroke={ANALYTICS_TOKENS.grid} strokeWidth={ANALYTICS_MARK.gridWidth} />
            {area ? <path d={area} fill={ANALYTICS_TOKENS.accentSoft} /> : null}
            {solidLine ? (
              <path
                d={solidLine}
                fill="none"
                stroke={ANALYTICS_TOKENS.accent}
                strokeWidth={ANALYTICS_MARK.lineWidth}
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            ) : null}
            {tailLine ? (
              <path
                d={tailLine}
                fill="none"
                stroke={ANALYTICS_TOKENS.accent}
                strokeWidth={ANALYTICS_MARK.lineWidth}
                strokeDasharray={ANALYTICS_MARK.tailDash}
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            ) : null}
          </svg>
          <div className={"mt-2 flex min-w-0 items-start justify-between gap-2 " + ANALYTICS_TEXT.captionTight}>
            <span className="shrink-0">Per-bucket halves</span>
            <span className="min-w-0 break-words text-right tabular-nums">{deltaHint}</span>
          </div>
        </div>
        <span
          className={"absolute inset-x-0 bottom-0 h-0.5 " + (selected ? "bg-fg" : "bg-transparent")}
          aria-hidden="true"
        />
      </button>
    </Card>
  );
}
