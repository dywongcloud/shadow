"use client";

import { useState } from "react";
import dynamic from "next/dynamic";
import { Calendar, ChevronDown, Coins, DollarSign, MoreHorizontal } from "lucide-react";

const BarChart = dynamic(() => import("@tremor/react").then((m) => m.BarChart), {
  ssr: false,
  loading: () => <div className="h-72 animate-pulse rounded-lg bg-subtle" aria-label="Loading chart" />,
});
const SparkAreaChart = dynamic(() => import("@tremor/react").then((m) => m.SparkAreaChart), {
  ssr: false,
  loading: () => <div className="h-7 w-28 animate-pulse rounded bg-subtle" />,
});
import { Card } from "@/components/ui";
import { usePoll, type FunctionStats, type BillingInfo, type Metrics, type UsageSummary } from "@/lib/api";
import { cn } from "@/lib/utils";

/** A flat sparkline at the real current level, for metrics tracked as a running total rather than a time series (CPU/memory/instances): it shows
 *  the real magnitude without fabricating a growth curve. */
function flat(value: number, n: number) {
  return Array.from({ length: n }, () => Math.round(value));
}

function fmt(n: number) {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(2) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1) + "K";
  return String(n);
}

type Gran = "Daily" | "Weekly" | "Monthly";

/** Each granularity maps to a real backend window + bucket resolution (`crates/hive-cloud/src/metrics.rs` `Granularity`), not a client relabel:
 *  `apiGran` selects the ring buffer (minute/hour/day) and `pollMs` matches how often that resolution can change. */
const GRAN: Record<Gran, { minutes: number; apiGran: "minute" | "hour" | "day"; pollMs: number }> = {
  Daily: { minutes: 1440, apiGran: "minute", pollMs: 60_000 },
  Weekly: { minutes: 10_080, apiGran: "hour", pollMs: 300_000 },
  Monthly: { minutes: 43_200, apiGran: "day", pollMs: 300_000 },
};

function bucketLabel(gran: Gran, t_ms: number) {
  const d = new Date(t_ms);
  if (gran === "Daily") return d.toLocaleTimeString("en-US", { hour: "numeric", minute: "2-digit" });
  if (gran === "Weekly") return d.toLocaleDateString("en-US", { weekday: "short", hour: "numeric" });
  return d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
}

export default function UsageView() {
  const { data: fns } = usePoll<FunctionStats[]>("/v1/functions", 15000);
  const { data: billing } = usePoll<BillingInfo>("/v1/billing", 30000);
  const [gran, setGran] = useState<Gran>("Monthly");
  const [cumulative, setCumulative] = useState(true);
  const { minutes: granMinutes, apiGran, pollMs } = GRAN[gran];
  const { data: metrics } = usePoll<Metrics>(`/v1/metrics?minutes=${granMinutes}&gran=${apiGran}`, pollMs);

  const requests = metrics?.totals?.requests ?? 0;
  const blocked = metrics?.totals?.blocked ?? 0;
  const cacheHits = (metrics?.series ?? []).reduce((a, b) => a + b.cache_hits, 0);
  const invocations = (fns ?? []).reduce((a, f) => a + f.requests, 0);
  const activeCpuMs = (fns ?? []).reduce((a, f) => a + (f.active_cpu_ms ?? 0), 0);
  const memGbHrs = (fns ?? []).reduce((a, f) => a + (f.memory_gb_hrs ?? 0), 0);
  const instances = (fns ?? []).reduce((a, f) => a + f.instances, 0);

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
  const planName = acc?.plan ?? "hobby";

  const buckets = metrics?.series ?? [];
  const perBucketEdge = buckets.map((b) => b.requests);
  const perBucketCache = buckets.map((b) => b.cache_hits);
  const perBucketBlocked = buckets.map((b) => b.blocked);
  const cum = (arr: number[]) => {
    let acc = 0;
    return arr.map((v) => (acc += v));
  };
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

  /** The five storage/network SKUs come ONLY from `/v1/usage/summary`, whose usage key is `usage`.
   *  A rejected session (401, or `x-hive-anon: 1` with an empty body) yields no payload, so `data`
   *  stays null, the section renders an empty state and NEVER seeds zeros. A node that predates a
   *  counter omits it, and an absent value renders as "-", never as a measured 0. */
  const { data: summary, error: summaryError } = usePoll<UsageSummary>("/v1/usage/summary", 30000);
  const ut = summary?.usage;
  const ur = summary?.rate_card;
  const summaryReady = !!summary && !!summary.usage && !!summary.rate_card;
  const GB = 1_000_000_000;
  const MS_MONTH = 30 * 24 * 3600 * 1000;
  const perM = (v?: number) => (v === undefined ? null : v / 1_000_000);
  const gb = (v?: number) => (v === undefined ? null : v / GB);
  const gbMonth = (v?: number) => (v === undefined ? null : v / (GB * MS_MONTH));
  const count = (v?: number) => (v === undefined ? "-" : fmt(v));
  const shown = (v: number | null, digits: number) => (v === null ? "-" : v.toFixed(digits));
  const price = (c: number | undefined, suffix: string) => (c === undefined ? "-" : `$${(c / 100).toFixed(3)}${suffix}`);
  const charge = (units: number | null, c?: number) => (units === null || c === undefined ? null : (units * c) / 100);
  const storageRows = [
    {
      key: "db-rows",
      name: "Database row ops",
      unit: "per million row ops",
      usage: count(ut?.db_row_ops),
      rate: price(ur?.db_row_ops_per_million_cents, "/M ops"),
      amount: charge(perM(ut?.db_row_ops), ur?.db_row_ops_per_million_cents),
    },
    {
      key: "db-storage",
      name: "Database storage",
      unit: "GB-month, from byte-ms",
      usage: shown(gbMonth(ut?.db_storage_byte_ms), 3),
      rate: price(ur?.db_storage_gb_month_cents, "/GB-mo"),
      amount: charge(gbMonth(ut?.db_storage_byte_ms), ur?.db_storage_gb_month_cents),
    },
    {
      key: "object-storage",
      name: "Object storage",
      unit: "GB-month, from byte-ms",
      usage: shown(gbMonth(ut?.object_storage_byte_ms), 3),
      rate: price(ur?.object_storage_gb_month_cents, "/GB-mo"),
      amount: charge(gbMonth(ut?.object_storage_byte_ms), ur?.object_storage_gb_month_cents),
    },
    {
      key: "ingress",
      name: "Ingress",
      unit: "GB transferred in",
      usage: shown(gb(ut?.ingress_bytes), 3),
      rate: price(ur?.ingress_gb_cents, "/GB"),
      amount: charge(gb(ut?.ingress_bytes), ur?.ingress_gb_cents),
    },
    {
      key: "egress",
      name: "Egress",
      unit: "GB transferred out",
      usage: shown(gb(ut?.egress_bytes), 3),
      rate: price(ur?.egress_gb_cents, "/GB"),
      amount: charge(gb(ut?.egress_bytes), ur?.egress_gb_cents),
    },
  ];

  return (
    <div>
      <h1 className="mb-5 text-2xl font-semibold capitalize tracking-tight">{planName} Plan Usage</h1>

      <div className="mb-4 flex flex-wrap items-center gap-2 text-sm">
        <Pill icon={<ChevronDown className="h-4 w-4" />}>Current Billing Cycle</Pill>
        <Pill icon={<Calendar className="h-4 w-4" />}>{billingRange()}</Pill>
        <Pill icon={<ChevronDown className="h-4 w-4" />}>All Products</Pill>
        <Pill icon={<ChevronDown className="h-4 w-4" />}>All Projects</Pill>
        <Pill icon={<ChevronDown className="h-4 w-4" />}>By Product</Pill>
        <button className="rounded-md border border-border px-2 py-2 text-secondary hover:bg-subtle"><MoreHorizontal className="h-4 w-4" /></button>
      </div>

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

      <Card>
        <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
          <h2 className="text-lg font-semibold">Consumption Breakdown</h2>
          <div className="flex items-center gap-3 sm:gap-4">
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
          <Row color="bg-cyan-500" name="Provisioned Instances" spark={flat(instances * 100, N)} usage={`${instances}`} charge={0} />
        </div>
      </Card>

      <Card className="mt-4">
        <h2 className="text-lg font-semibold">Storage &amp; network</h2>
        <p className="mb-4 mt-1 text-sm text-secondary">
          Five metered SKUs from your usage summary. Rates are the public rate card; these sit apart from the compute totals above.
        </p>
        {!summaryReady || summaryError ? (
          <div className="rounded-lg border border-dashed border-border px-4 py-8 text-center text-sm text-secondary">
            {summaryError
              ? "Usage summary is unavailable for this session. Sign in to see storage and network usage."
              : "No storage or network usage reported for this billing cycle."}
          </div>
        ) : (
          <div>
            <div className="grid grid-cols-[1fr_auto_auto_auto] gap-2 border-b border-border pb-2 text-xs font-medium uppercase tracking-wide text-muted sm:gap-4">
              <span>SKU</span><span className="text-right">Usage</span><span className="text-right">Rate</span><span className="text-right">Charge</span>
            </div>
            {storageRows.map((r) => (
              <div key={r.key} className="grid grid-cols-[1fr_auto_auto_auto] items-center gap-2 border-b border-border py-3 text-sm sm:gap-4">
                <span className="min-w-0">
                  <span className="block truncate text-fg">{r.name}</span>
                  <span className="block text-xs text-secondary">{r.unit}</span>
                </span>
                <span className="w-20 text-right tabular-nums text-secondary sm:w-24">{r.usage}</span>
                <span className="w-24 text-right tabular-nums text-secondary sm:w-28">{r.rate}</span>
                <span className="w-16 text-right tabular-nums">{r.amount === null ? "-" : `$${r.amount.toFixed(2)}`}</span>
              </div>
            ))}
          </div>
        )}
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
    <div className="grid grid-cols-[1fr_auto_auto] items-center gap-2 border-b border-border py-3 sm:gap-4">
      <span className="flex min-w-0 items-center gap-2.5 text-sm">
        <span className={cn("h-2.5 w-2.5 shrink-0 rounded-full", color)} />
        <span className="truncate">{name}</span>
      </span>
      <span className="flex items-center justify-end gap-3">
        <SparkAreaChart data={data} index="i" categories={["v"]} colors={["slate"]} className="hidden h-7 w-28 sm:block" />
        <span className="w-20 text-right text-sm tabular-nums text-secondary sm:w-28">{usage}</span>
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
