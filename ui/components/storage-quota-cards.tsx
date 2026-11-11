"use client";

/** Storage index usage cards. Every printed number comes from a field the platform returns:
 *  `GET /v1/databases` supplies the managed ids and the count; `GET /v1/databases/:id/metrics`
 *  supplies `counters.requests`, `counters.bytes_in`, `counters.bytes_out`, the byte gauges
 *  in `stats` and `observed_at_ms`; `GET /v1/billing` supplies `current_invoice.total_cents`,
 *  `current_invoice.status`, `account.used_cents` and `account.included_cents`.
 *  The operator-only metering endpoint is never called from this tenant-facing surface.
 *  No endpoint reports an allowance for requests, bandwidth or stored bytes, so those three
 *  cards carry no progress bar. The one bar uses two real billing fields, never an invented
 *  ceiling, and is omitted when either field is absent. */

import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { Activity, ArrowUpDown, Database, DollarSign, Gauge, HardDrive } from "lucide-react";
import { Badge, Card, StateNotice } from "@/components/ui";
import { usePoll, type BillingInfo, type DbInstanceMetrics } from "@/lib/api";

const METRICS_POLL_MS = 20_000;
const BILLING_POLL_MS = 30_000;
const ABSENT = "—";

const COLOR = {
  databases: "#a78bfa",
  requests: "#6366f1",
  bandwidth: "#22d3ee",
  storage: "#34d399",
  cost: "#fbbf24",
  costEnd: "#fb7185",
} as const;

/** Only a finite numeric field is a measurement. A refused poll, an older node or a missing
 *  optional key stays absent, never a fabricated zero. */
function measured(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function fmtCount(n: number | null): string {
  return n === null ? ABSENT : new Intl.NumberFormat("en-US").format(n);
}

function fmtBytes(n: number | null): string {
  if (n === null) return ABSENT;
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let value = n / 1024;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i += 1;
  }
  const digits = value >= 100 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${units[i]}`;
}

function fmtCents(n: number | null): string {
  return n === null ? ABSENT : `$${(n / 100).toFixed(2)}`;
}

interface Sample {
  requests: number | null;
  bytes: number | null;
  storageBytes: number | null;
  observedAtMs: number | null;
}

/** One per-instance answer. `stats.bytes` is a Postgres/Supabase database size or a blob
 *  bucket's object bytes; `stats.file_bytes` is a managed SQLite file set. Engines with no
 *  byte gauge in `stats` contribute no storage value, and the card says how many did. */
function sampleOf(m: DbInstanceMetrics): Sample {
  const c = m.counters;
  const inbound = measured(c?.bytes_in);
  const outbound = measured(c?.bytes_out);
  const stats = m.stats ?? undefined;
  return {
    requests: measured(c?.requests),
    bytes: inbound !== null && outbound !== null ? inbound + outbound : null,
    storageBytes: measured(stats?.file_bytes) ?? measured(stats?.bytes),
    observedAtMs: measured(m.observed_at_ms),
  };
}

function sameSample(a: Sample, b: Sample): boolean {
  return (
    a.requests === b.requests &&
    a.bytes === b.bytes &&
    a.storageBytes === b.storageBytes &&
    a.observedAtMs === b.observedAtMs
  );
}

/** One hook per database: a hook cannot run in a loop, so each id gets a component. It renders
 *  nothing and owns no layout, so the page's mobile grid never inherits an overflow. */
function MetricProbe({ id, onSample }: { id: string; onSample: (id: string, sample: Sample | null) => void }) {
  const { data, error } = usePoll<DbInstanceMetrics>(
    `/v1/databases/${encodeURIComponent(id)}/metrics`,
    METRICS_POLL_MS,
  );
  useEffect(() => {
    onSample(id, data && !error ? sampleOf(data) : null);
  }, [data, error, id, onSample]);
  return null;
}

function QuotaCard({
  label,
  value,
  caption,
  color,
  endColor,
  icon,
  badge,
  ratio,
  ratioCaption,
}: {
  label: string;
  value: string;
  caption: string;
  color: string;
  endColor?: string;
  icon: ReactNode;
  badge?: ReactNode;
  ratio?: number | null;
  ratioCaption?: string;
}) {
  const pct =
    ratio !== null && ratio !== undefined && Number.isFinite(ratio)
      ? Math.max(0, Math.min(ratio, 1)) * 100
      : null;
  const strip = `linear-gradient(90deg, ${color} 0%, ${endColor ?? color} 48%, ${color}00 100%)`;
  const mask = "linear-gradient(90deg, #000 0%, #000 68%, transparent 100%)";
  return (
    <Card className="relative flex min-w-0 flex-col gap-2 overflow-hidden p-4 sm:p-5">
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-x-0 top-0 h-[3px]"
        style={{ backgroundImage: strip, WebkitMaskImage: mask, maskImage: mask }}
      />
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-x-0 bottom-0 h-px"
        style={{ backgroundImage: `linear-gradient(90deg, ${color}55, ${color}00)` }}
      />
      <div className="flex min-w-0 items-center justify-between gap-2">
        <span className="flex min-w-0 items-center gap-1.5 text-[11px] font-semibold uppercase leading-tight tracking-[0.12em] text-muted">
          <span className="shrink-0" style={{ color }}>{icon}</span>
          <span className="min-w-0 break-words">{label}</span>
        </span>
        {badge ?? null}
      </div>
      <div className="min-w-0 break-words text-2xl font-semibold leading-tight tabular-nums text-fg sm:text-[1.75rem]">
        {value}
      </div>
      <p className="m-0 min-w-0 break-words text-xs leading-snug text-secondary">{caption}</p>
      {pct !== null ? (
        <div className="mt-1 min-w-0">
          <div
            className="h-1.5 w-full min-w-0 overflow-hidden rounded-full bg-subtle"
            role="progressbar"
            aria-valuenow={Math.round(pct)}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-label={ratioCaption ?? label}
          >
            <div
              className="h-full rounded-full"
              style={{ width: `${pct}%`, backgroundImage: `linear-gradient(90deg, ${color}, ${endColor ?? color})` }}
            />
          </div>
          <p className="m-0 mt-1 min-w-0 break-words text-[11px] leading-snug text-muted">{ratioCaption}</p>
        </div>
      ) : null}
    </Card>
  );
}

/** A row of measured usage cards at the top of the storage index. The only denominator shown
 *  anywhere is the plan's real `included_cents`, next to the real `used_cents`. */
export function StorageQuotaCards({ managedIds }: { managedIds: string[] }) {
  const idsKey = JSON.stringify(Array.from(new Set(managedIds)).sort());
  const ids = useMemo(() => JSON.parse(idsKey) as string[], [idsKey]);
  const [samples, setSamples] = useState<Record<string, Sample>>({});

  const onSample = useCallback((id: string, sample: Sample | null) => {
    setSamples((prev) => {
      if (sample === null) {
        if (!(id in prev)) return prev;
        const next = { ...prev };
        delete next[id];
        return next;
      }
      const old = prev[id];
      if (old && sameSample(old, sample)) return prev;
      return { ...prev, [id]: sample };
    });
  }, []);

  // A team switch invalidates every sample: never paint one tenant's counters over another's.
  useEffect(() => {
    const clear = () => setSamples({});
    window.addEventListener("hive-team-changed", clear);
    return () => window.removeEventListener("hive-team-changed", clear);
  }, []);

  const { data: billing } = usePoll<BillingInfo>("/v1/billing", BILLING_POLL_MS);

  const reporting = ids
    .map((id) => samples[id])
    .filter((s): s is Sample => Boolean(s));
  const countersComplete =
    reporting.length > 0 && reporting.every((s) => s.requests !== null && s.bytes !== null);
  const requests = countersComplete
    ? reporting.reduce((total, s) => total + (s.requests ?? 0), 0)
    : null;
  const bandwidth = countersComplete
    ? reporting.reduce((total, s) => total + (s.bytes ?? 0), 0)
    : null;
  const storageSamples = reporting.filter((s) => s.storageBytes !== null);
  const storage = storageSamples.length
    ? storageSamples.reduce((total, s) => total + (s.storageBytes ?? 0), 0)
    : null;
  const sampledAt = reporting.reduce<number | null>(
    (latest, s) => (s.observedAtMs === null ? latest : Math.max(latest ?? 0, s.observedAtMs)),
    null,
  );

  const invoiceCents = measured(billing?.current_invoice?.total_cents);
  const invoiceStatus = billing?.current_invoice?.status;
  const usedCents = measured(billing?.account?.used_cents);
  const includedCents = measured(billing?.account?.included_cents);
  const planRatio =
    usedCents !== null && includedCents !== null && includedCents > 0
      ? usedCents / includedCents
      : null;
  const scope = ids.length
    ? `${reporting.length} of ${ids.length} databases reporting`
    : "no managed databases to measure";

  return (
    <section aria-label="Storage usage" className="mb-6 min-w-0 max-w-full">
      {ids.map((id) => (
        <MetricProbe key={id} id={id} onSample={onSample} />
      ))}
      <div className="grid min-w-0 max-w-full grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-6">
        <QuotaCard
          label="Databases"
          value={fmtCount(ids.length)}
          caption="managed databases · no plan allowance reported"
          color={COLOR.databases}
          icon={<Database className="h-3.5 w-3.5" aria-hidden="true" />}
        />
        <QuotaCard
          label="Requests"
          value={fmtCount(requests)}
          caption={`gateway request counters · ${scope} · no plan allowance reported`}
          color={COLOR.requests}
          icon={<Activity className="h-3.5 w-3.5" aria-hidden="true" />}
        />
        <QuotaCard
          label="Bandwidth"
          value={fmtBytes(bandwidth)}
          caption="ingress + egress bytes from the same counters · no plan allowance reported"
          color={COLOR.bandwidth}
          icon={<ArrowUpDown className="h-3.5 w-3.5" aria-hidden="true" />}
        />
        <QuotaCard
          label="Storage"
          value={fmtBytes(storage)}
          caption={
            ids.length
              ? `reported byte sizes from ${storageSamples.length} of ${ids.length} databases · no plan allowance reported`
              : "no managed databases to measure · no plan allowance reported"
          }
          color={COLOR.storage}
          icon={<HardDrive className="h-3.5 w-3.5" aria-hidden="true" />}
        />
        <QuotaCard
          label="Plan usage"
          value={fmtCents(usedCents)}
          caption={
            planRatio === null
              ? "metered usage this period · no plan allowance reported"
              : "metered usage this period · real plan allowance reported"
          }
          color={COLOR.cost}
          endColor={COLOR.costEnd}
          icon={<Gauge className="h-3.5 w-3.5" aria-hidden="true" />}
          ratio={planRatio}
          ratioCaption={
            planRatio === null
              ? undefined
              : `of ${fmtCents(includedCents)} included credit`
          }
        />
        <QuotaCard
          label="Invoice"
          value={fmtCents(invoiceCents)}
          caption="current period invoice · no plan allowance reported"
          color={COLOR.costEnd}
          endColor={COLOR.cost}
          icon={<DollarSign className="h-3.5 w-3.5" aria-hidden="true" />}
          badge={
            invoiceStatus ? (
              <Badge tone="amber" className="shrink-0">{invoiceStatus}</Badge>
            ) : null
          }
        />
      </div>
      <p className="mt-3 min-w-0 break-words text-xs leading-snug text-muted">
        Per-instance counters are not persisted and reset on restart ·{" "}
        {sampledAt === null
          ? "observation time not reported"
          : `sampled ${new Date(sampledAt).toISOString().replace("T", " ").slice(0, 19)} UTC`}
      </p>
      {ids.length > 0 && reporting.length === 0 ? (
        <StateNotice tone="muted" className="mt-3">
          No database metrics have answered yet; measured values stay absent until one does.
        </StateNotice>
      ) : null}
    </section>
  );
}
