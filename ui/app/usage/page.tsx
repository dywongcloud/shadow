"use client";

import { useState } from "react";
import dynamic from "next/dynamic";
import { Calendar, ChevronDown, Coins, DollarSign, MoreHorizontal } from "lucide-react";

// Lazy-load Tremor charts so the heavy charting bundle is split out of the page.
const BarChart = dynamic(() => import("@tremor/react").then((m) => m.BarChart), { ssr: false });
const SparkAreaChart = dynamic(() => import("@tremor/react").then((m) => m.SparkAreaChart), { ssr: false });
import { Card } from "@/components/ui";
import { usePoll, type Overview, type FunctionStats, type BillingInfo, type Metrics } from "@/lib/api";
import { cn } from "@/lib/utils";

// A flat sparkline at the real current level — used for metrics we track as a
// running total but NOT as a time-series (CPU/memory/instances). Honest: it shows
// the real magnitude without fabricating a growth curve we don't have data for.
function flat(value: number, n: number) {
  return Array.from({ length: n }, () => Math.round(value));
}

function fmt(n: number) {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(2) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1) + "K";
  return String(n);
}

type Gran = "Daily" | "Weekly" | "Monthly";

// Each granularity maps to a real backend window + bucket resolution
// (crates/hive-cloud/src/metrics.rs's Granularity enum/MAX_*_BUCKETS) — NOT a
// client-side relabeling of the same 3-hour fetch. `minutes` is the span
// requested; `apiGran` selects which ring buffer the backend reads from
// (minute/hour/day); `pollMs` matches how often that resolution can actually
// change (polling an hourly rollup every 15s wastes a request 239/240 times).
const GRAN: Record<Gran, { minutes: number; apiGran: "minute" | "hour" | "day"; pollMs: number }> = {
  Daily: { minutes: 1440, apiGran: "minute", pollMs: 60_000 },
  Weekly: { minutes: 10_080, apiGran: "hour", pollMs: 300_000 },
  Monthly: { minutes: 43_200, apiGran: "day", pollMs: 300_000 },
};

// Bucket label formatting keyed on granularity — an hourly bucket over 7 days
// needs a weekday+hour label, a daily bucket over 30 days needs a month/day
// label; both would otherwise render via the Daily hour:minute formatter and
// (for Monthly specifically) collapse to the SAME "12:00 AM" label on every
// point, since a day-floored t_ms always lands on local midnight.
function bucketLabel(gran: Gran, t_ms: number) {
  const d = new Date(t_ms);
  if (gran === "Daily") return d.toLocaleTimeString("en-US", { hour: "numeric", minute: "2-digit" });
  if (gran === "Weekly") return d.toLocaleDateString("en-US", { weekday: "short", hour: "numeric" });
  return d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
}

export default function UsagePage() {
  const { data: ov } = usePoll<Overview>("/v1/overview", 3000);
  const { data: fns } = usePoll<FunctionStats[]>("/v1/functions", 3000);
  const { data: billing } = usePoll<BillingInfo>("/v1/billing", 5000);
  const [gran, setGran] = useState<Gran>("Daily");
  const [cumulative, setCumulative] = useState(true);
  const { minutes: granMinutes, apiGran, pollMs } = GRAN[gran];
  // Real per-tenant traffic time-series, bucketed at the resolution the active
  // Daily/Weekly/Monthly toggle selects. The scoped `/v1/metrics` backs the
  // consumption chart with ACTUAL requests/cache/blocked over time — not a
  // fabricated curve — at whichever of the three real backend resolutions
  // (crates/hive-cloud/src/metrics.rs) `gran` is currently set to.
  const { data: metrics } = usePoll<Metrics>(`/v1/metrics?minutes=${granMinutes}&gran=${apiGran}`, pollMs);

  const requests = ov?.requests ?? 0;
  const blocked = ov?.blocked ?? 0;
  const cacheHits = ov?.cdn.hits ?? 0;
  const invocations = (fns ?? []).reduce((a, f) => a + f.requests, 0);
  // Active CPU pricing (Vercel Fluid convention): bill ACTIVE CPU time + provisioned
  // MEMORY GB-hrs, not idle instance wall-time. `active_cpu_ms` excludes I/O-idle
  // keep-warm time — that's where the headline savings come from.
  const activeCpuMs = (fns ?? []).reduce((a, f) => a + (f.active_cpu_ms ?? 0), 0);
  const memGbHrs = (fns ?? []).reduce((a, f) => a + (f.memory_gb_hrs ?? 0), 0);

  // Charges are computed from the AUTHORITATIVE rate card the billing API returns
  // (crates/hive-cloud/src/billing.rs RATE_CARD) — never hardcoded multipliers.
  // rate_card cents are: per active-CPU-hour, per memory-GB-hour, per 1M requests,
  // per 1M WAF-blocked. Fall back to 0 (shows $0.00) until the card loads, rather
  // than inventing a price.
  const rc = billing?.rate_card;
  const edgeCharge = (requests / 1_000_000) * ((rc?.requests_per_million_cents ?? 0) / 100);
  const cpuCharge = (activeCpuMs / 1000 / 3600) * ((rc?.active_cpu_hr_cents ?? 0) / 100);
  const memCharge = memGbHrs * ((rc?.mem_gb_hr_cents ?? 0) / 100);
  const fnCharge = cpuCharge + memCharge;
  const wafCharge = (blocked / 1_000_000) * ((rc?.waf_per_million_cents ?? 0) / 100);
  const acc = billing?.account;
  const includedTotal = acc ? acc.included_cents / 100 : 20;
  const includedUsed = acc ? acc.used_cents / 100 : Math.min(includedTotal, edgeCharge + fnCharge + wafCharge);
  const onDemand = acc ? Math.max(0, -acc.balance_cents) / 100 : edgeCharge + fnCharge + wafCharge;
  const creditBalance = acc ? acc.balance_cents / 100 : 0;
  const planName = (acc?.plan ?? (ov?.concurrency.plan === "enterprise" ? "enterprise" : "hobby"));

  // REAL per-tenant time-series from the metrics buckets (per-minute). Edge
  // requests, cache hits and blocked requests are tracked over time, so their
  // charts + sparklines are actual data. Invocations/CPU/memory/instances are
  // running totals with no time-series → flat sparks at the real level.
  const buckets = metrics?.series ?? [];
  const perBucketEdge = buckets.map((b) => b.requests);
  const perBucketCache = buckets.map((b) => b.cache_hits);
  const perBucketBlocked = buckets.map((b) => b.blocked);
  const cum = (arr: number[]) => {
    let acc = 0;
    return arr.map((v) => (acc += v));
  };
  // Sparklines mirror the chart mode; cumulative = running sum of the real buckets.
  const edgeS = cumulative ? cum(perBucketEdge) : perBucketEdge;
  const cacheS = cumulative ? cum(perBucketCache) : perBucketCache;
  const blockedS = cumulative ? cum(perBucketBlocked) : perBucketBlocked;
  const N = Math.max(buckets.length, 1);
  const fnS = flat(invocations, N);
  const chart = buckets.map((b, i) => ({
    date: bucketLabel(gran, b.t_ms),
    "Edge Requests": edgeS[i] ?? 0,
    "Cache Hits": cacheS[i] ?? 0,
    "Blocked": blockedS[i] ?? 0,
  }));

  const pct = Math.min(100, includedTotal ? (includedUsed / includedTotal) * 100 : 0);

  return (
    <div>
      <h1 className="mb-5 text-2xl font-semibold capitalize tracking-tight">{planName} Plan Usage</h1>

      {/* Filter row */}
      <div className="mb-4 flex flex-wrap items-center gap-2 text-sm">
        <Pill icon={<ChevronDown className="h-4 w-4" />}>Current Billing Cycle</Pill>
        <Pill icon={<Calendar className="h-4 w-4" />}>{billingRange()}</Pill>
        <Pill icon={<ChevronDown className="h-4 w-4" />}>All Products</Pill>
        <Pill icon={<ChevronDown className="h-4 w-4" />}>All Projects</Pill>
        <Pill icon={<ChevronDown className="h-4 w-4" />}>By Product</Pill>
        <button className="rounded-md border border-border px-2 py-2 text-secondary hover:bg-subtle"><MoreHorizontal className="h-4 w-4" /></button>
      </div>

      {/* Credit + on-demand */}
      <Card className="mb-4">
        <div className="flex items-start justify-between">
          <div className="flex items-center gap-3">
            <span className="flex h-9 w-9 items-center justify-center rounded-lg bg-blue-500/10 text-blue-500"><Coins className="h-4 w-4" /></span>
            <div>
              <div className="text-sm text-secondary">Included Credit</div>
              <div className="text-lg font-semibold tabular-nums">${includedUsed.toFixed(2)} / ${includedTotal.toFixed(2)}</div>
            </div>
            <div className="ml-6 border-l border-border pl-6">
              <div className="text-sm text-secondary">Credit Balance</div>
              <div className={cn("text-lg font-semibold tabular-nums", creditBalance < 0 && "text-red-500")}>${creditBalance.toFixed(2)}</div>
            </div>
          </div>
          <div className="flex items-center gap-3 text-right">
            <div>
              <div className="text-sm text-secondary">On-Demand Charges</div>
              <div className="text-lg font-semibold tabular-nums">${onDemand.toFixed(2)}</div>
            </div>
            <span className="flex h-9 w-9 items-center justify-center rounded-full bg-purple-500/10 text-purple-500"><DollarSign className="h-4 w-4" /></span>
          </div>
        </div>
        <div className="mt-4 flex h-2.5 overflow-hidden rounded-full bg-subtle">
          <div className="h-full bg-[#0070f3]" style={{ width: `${pct}%` }} />
          <div className="h-full bg-[#7928ca]" style={{ width: `${100 - pct}%` }} />
        </div>
      </Card>

      {/* Consumption breakdown */}
      <Card>
        <div className="mb-4 flex items-center justify-between">
          <h2 className="text-lg font-semibold">Consumption Breakdown</h2>
          <div className="flex items-center gap-4">
            <div className="flex rounded-lg border border-border p-0.5 text-sm">
              {(["Daily", "Weekly", "Monthly"] as const).map((g) => (
                <button
                  key={g}
                  onClick={() => setGran(g)}
                  className={cn("rounded-md px-2.5 py-1", gran === g ? "bg-subtle font-medium text-fg" : "text-secondary")}
                >
                  {g}
                </button>
              ))}
            </div>
            <label className="flex cursor-pointer items-center gap-2 text-sm text-secondary">
              <input type="checkbox" checked={cumulative} onChange={(e) => setCumulative(e.target.checked)} className="h-4 w-4 accent-fg" />
              Cumulative
            </label>
          </div>
        </div>

        <BarChart
          data={chart}
          index="date"
          categories={["Edge Requests", "Cache Hits", "Blocked"]}
          colors={["blue", "emerald", "rose"]}
          stack
          showLegend={false}
          showAnimation
          valueFormatter={(v) => fmt(v)}
          className="h-72"
        />

        {/* Product table */}
        <div className="mt-6">
          <div className="grid grid-cols-[1fr_auto_auto] gap-4 border-b border-border pb-2 text-xs font-medium uppercase tracking-wide text-muted">
            <span>Product</span><span className="text-right">Usage</span><span className="text-right">Charge</span>
          </div>

          <GroupHeader name="OpenEdge Delivery Network" />
          <Row color="bg-blue-500" name="Edge Requests" spark={edgeS} usage={`${fmt(requests)}`} charge={edgeCharge} />
          <Row color="bg-emerald-500" name="Cache Hits" spark={cacheS} usage={`${fmt(cacheHits)}`} charge={0} />
          <Row color="bg-rose-500" name="Firewall — Blocked Requests" spark={blockedS} usage={`${fmt(blocked)}`} charge={wafCharge} />

          <GroupHeader name="OpenEdge Functions (Fluid · Active CPU pricing)" />
          <Row color="bg-amber-500" name="Function Invocations" spark={fnS} usage={`${fmt(invocations)}`} charge={0} />
          <Row color="bg-purple-500" name="Active CPU" spark={flat(activeCpuMs, N)} usage={`${(activeCpuMs / 1000).toFixed(1)} s`} charge={cpuCharge} />
          <Row color="bg-teal-500" name="Provisioned Memory" spark={flat(Math.round(memGbHrs * 1000), N)} usage={`${memGbHrs.toFixed(3)} GB-hr`} charge={memCharge} />
          <Row color="bg-cyan-500" name="Provisioned Instances" spark={flat((ov?.instances ?? 0) * 100, N)} usage={`${ov?.instances ?? 0}`} charge={0} />
        </div>
      </Card>
    </div>
  );
}

function Pill({ icon, children }: { icon: React.ReactNode; children: React.ReactNode }) {
  return (
    <button className="flex items-center gap-2 rounded-md border border-border bg-card px-3 py-2 text-secondary hover:bg-subtle">
      {icon}
      <span className="text-fg">{children}</span>
    </button>
  );
}

function GroupHeader({ name }: { name: string }) {
  return <div className="border-b border-border bg-subtle/60 px-1 py-2 text-sm font-medium text-secondary">{name}</div>;
}

function Row({ color, name, spark, usage, charge }: { color: string; name: string; spark: number[]; usage: string; charge: number }) {
  const data = spark.map((v, i) => ({ i, v }));
  return (
    <div className="grid grid-cols-[1fr_auto_auto] items-center gap-4 border-b border-border py-3">
      <span className="flex items-center gap-2.5 text-sm">
        <span className={cn("h-2.5 w-2.5 rounded-full", color)} />
        {name}
      </span>
      <span className="flex items-center justify-end gap-3">
        <SparkAreaChart data={data} index="i" categories={["v"]} colors={["slate"]} className="h-7 w-28" />
        <span className="w-28 text-right text-sm tabular-nums text-secondary">{usage}</span>
      </span>
      <span className="w-16 text-right text-sm tabular-nums">${charge.toFixed(2)}</span>
    </div>
  );
}

function billingRange() {
  const now = new Date();
  const start = new Date(now.getFullYear(), now.getMonth(), 1);
  const end = new Date(now.getFullYear(), now.getMonth() + 1, 1);
  const f = (d: Date) => d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
  return `${f(start)} - ${f(end)}, 12am`;
}
