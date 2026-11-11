"use client";

import { useId } from "react";
import { Activity, ArrowUpDown, Gauge, HardDrive } from "lucide-react";
import { Card } from "@/components/ui";
import type { DbInstanceMetrics } from "@/lib/api";
import { ABSENT, DB_COLORS, dbBytes, dbFinite, dbPlain, dbUtc, type DbColorRole } from "./db-chart";

/** Per-database usage cards. Every number comes from one `GET /v1/databases/:id/metrics`
 *  response: `counters.requests`, `counters.bytes_in` + `counters.bytes_out`, the byte gauge in
 *  `stats` (`bytes` for an engine or `file_bytes` for a managed SQLite set) and
 *  `counters.error_rate`. No endpoint reports a per-database cost, an included allowance or a
 *  quota, so no card invents a ceiling and no card draws a progress bar. The tenant's billing
 *  allowance is account-wide and is deliberately not attributed to one database here. */
interface QuotaCard {
  id: string;
  label: string;
  icon: typeof Activity;
  role: DbColorRole;
  value: string | null;
  caption: string;
}

function cardsFor(data: DbInstanceMetrics | null): QuotaCard[] {
  const c = data?.counters;
  const stats = data?.stats ?? undefined;
  const requests = dbFinite(c?.requests);
  const bytesIn = dbFinite(c?.bytes_in);
  const bytesOut = dbFinite(c?.bytes_out);
  const bandwidth = bytesIn !== null && bytesOut !== null ? bytesIn + bytesOut : null;
  const size = dbFinite(stats?.bytes) ?? dbFinite(stats?.file_bytes);
  const sizeKey = dbFinite(stats?.bytes) !== null ? "bytes" : dbFinite(stats?.file_bytes) !== null ? "file_bytes" : null;
  const rate = dbFinite(c?.error_rate);
  return [
    {
      id: "requests",
      label: "Requests",
      icon: Activity,
      role: "indigo",
      value: requests === null ? null : dbPlain(requests),
      caption: "Measured gateway requests since this node began counting. No per-database allowance is reported.",
    },
    {
      id: "bandwidth",
      label: "Bandwidth",
      icon: ArrowUpDown,
      role: "cyan",
      value: bandwidth === null ? null : dbBytes(bandwidth),
      caption: `Ingress ${dbBytes(bytesIn)} plus egress ${dbBytes(bytesOut)}. No per-database allowance is reported.`,
    },
    {
      id: "storage",
      label: "Storage",
      icon: HardDrive,
      role: "emerald",
      value: size === null ? null : dbBytes(size),
      caption: sizeKey
        ? `Current size from the node's ${sizeKey} gauge. No per-database allowance is reported.`
        : "No size gauge is reported for this database kind. Counters above remain measured.",
    },
    {
      id: "error-rate",
      label: "Error rate",
      icon: Gauge,
      role: "rose",
      value: rate === null ? null : `${(Math.max(0, Math.min(1, rate)) * 100).toFixed(1)}%`,
      caption: "Share of measured gateway requests that errored. No threshold or allowance is reported.",
    },
  ];
}

function CardFace({ card }: { card: QuotaCard }) {
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const role = DB_COLORS[card.role];
  const gradientId = `${uid}-wash`;
  const Icon = card.icon;
  return (
    <Card className="relative min-w-0 w-full max-w-full overflow-hidden p-0">
      <div
        className="absolute inset-x-0 top-0 h-1"
        style={{ background: `linear-gradient(90deg, ${role.lit}, ${role.color} 55%, transparent)` }}
        aria-hidden="true"
      />
      <svg viewBox="0 0 240 160" preserveAspectRatio="none" className="pointer-events-none absolute inset-0 h-full w-full" aria-hidden="true">
        <defs>
          <linearGradient id={gradientId} x1="0" y1="0" x2="1" y2="1">
            <stop offset="0%" style={{ stopColor: role.lit, stopOpacity: 0.16 }} />
            <stop offset="72%" style={{ stopColor: role.color, stopOpacity: 0.02 }} />
          </linearGradient>
        </defs>
        <path d="M 0 0 L 240 0 L 240 160 L 0 160 Z" fill={`url(#${gradientId})`} />
      </svg>
      <div className="relative min-w-0 p-4 sm:p-5">
        <div className="flex min-w-0 items-center gap-1.5 text-xs font-medium uppercase tracking-wide text-muted">
          <Icon className="h-3.5 w-3.5 shrink-0" style={{ color: role.color }} aria-hidden="true" />
          <span className="min-w-0 break-words">{card.label}</span>
        </div>
        <div className="mt-2 min-w-0 break-words text-2xl font-semibold leading-tight tabular-nums text-fg sm:text-3xl">
          {card.value ?? ABSENT}
        </div>
        <p className="mt-3 min-w-0 break-words text-xs leading-snug text-secondary">{card.caption}</p>
      </div>
    </Card>
  );
}

export function DbQuotaCards({ data }: { data: DbInstanceMetrics | null }) {
  const cards = cardsFor(data);
  const observed = dbFinite(data?.observed_at_ms);
  return (
    <section aria-label="Measured usage" className="min-w-0 w-full max-w-full">
      <div className="grid min-w-0 w-full max-w-full grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
        {cards.map((card) => <CardFace key={card.id} card={card} />)}
      </div>
      <p className="mt-3 min-w-0 break-words text-xs leading-snug text-muted">
        Values are measured on the node that answered and reset when that process restarts. No cost or included allowance is attributed to a single database: the platform reports billing for the account, not per database.
        {observed !== null ? ` Reading taken ${dbUtc(observed)}.` : ""}
      </p>
    </section>
  );
}
