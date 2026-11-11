"use client";

import { useId } from "react";
import { Badge, Card } from "@/components/ui";

export type MetricDeltaTone = "default" | "green" | "red" | "amber" | "purple";

const W = 240;
const H = 64;
const PAD = 4;

function finite(value: number | null | undefined): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

export function AnalyticsMetricCard({
  label,
  value,
  values,
  delta,
  deltaTone,
  deltaHint,
  deltaAria,
  color,
  lit,
}: {
  label: string;
  value: string;
  values: number[];
  delta: string;
  deltaTone: MetricDeltaTone;
  deltaHint: string;
  deltaAria: string;
  color: string;
  lit: string;
}) {
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const gradientId = uid + "-metric-fill";
  const clean = values.map(finite);
  let max = 0;
  for (const item of clean) if (item > max) max = item;
  const step = clean.length > 1 ? (W - 2 * PAD) / (clean.length - 1) : 0;
  const yFor = (item: number) => H - PAD - (max > 0 ? (item / max) * (H - 2 * PAD) : 0);
  const points = clean.map((item, index) => ({ x: PAD + step * index, y: yFor(item) }));
  const line = points.length
    ? points.map((point, index) => (index === 0 ? "M " : "L ") + point.x.toFixed(2) + " " + point.y.toFixed(2)).join(" ")
    : "M " + PAD + " " + (H - PAD);
  const area = points.length > 1
    ? line + " L " + points[points.length - 1].x.toFixed(2) + " " + (H - PAD) + " L " + points[0].x.toFixed(2) + " " + (H - PAD) + " Z"
    : "";

  return (
    <Card className="relative min-w-0 w-full max-w-full overflow-hidden p-0">
      <div
        className="absolute inset-x-0 top-0 h-1"
        style={{ background: "linear-gradient(90deg, " + lit + ", " + color + ")" }}
        aria-hidden="true"
      />
      <div className="min-w-0 p-4 sm:p-5">
        <div className="flex min-w-0 items-start justify-between gap-2">
          <div className="min-w-0">
            <div className="min-w-0 break-words text-xs font-medium uppercase leading-tight tracking-wide text-muted">{label}</div>
            <div className="mt-1 min-w-0 break-words text-2xl font-semibold leading-tight tabular-nums text-fg sm:text-3xl">{value}</div>
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
          <defs>
            <linearGradient id={gradientId} x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" style={{ stopColor: lit, stopOpacity: 0.42 }} />
              <stop offset="100%" style={{ stopColor: color, stopOpacity: 0.02 }} />
            </linearGradient>
          </defs>
          <line x1={PAD} y1={H - PAD} x2={W - PAD} y2={H - PAD} stroke="hsl(var(--border))" strokeWidth="1" />
          {area ? <path d={area} fill={"url(#" + gradientId + ")"} /> : null}
          <path d={line} fill="none" style={{ stroke: lit }} strokeOpacity="0.18" strokeWidth="8" strokeLinecap="round" strokeLinejoin="round" />
          <path d={line} fill="none" style={{ stroke: color }} strokeWidth="2.25" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
        <div className="mt-2 flex min-w-0 items-start justify-between gap-2 text-xs text-secondary">
          <span className="shrink-0">Per-bucket halves</span>
          <span className="min-w-0 break-words text-right tabular-nums">{deltaHint}</span>
        </div>
      </div>
    </Card>
  );
}
