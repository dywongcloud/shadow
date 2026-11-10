"use client";

import { useState } from "react";
import dynamic from "next/dynamic";
import { Calendar, ChevronDown, Coins, DollarSign, MoreHorizontal } from "lucide-react";

const BarChart = dynamic(() => import("@tremor/react").then((m) => m.BarChart), {
  ssr: false,
  loading: () => <div className="h-72 w-full max-w-full animate-pulse rounded-lg bg-subtle" aria-label="Loading chart" />,
});
const SparkAreaChart = dynamic(() => import("@tremor/react").then((m) => m.SparkAreaChart), {
  ssr: false,
  loading: () => <div className="h-7 w-full max-w-full animate-pulse rounded bg-subtle" />,
});
import { Card } from "@/components/ui";
import { usePoll, type FunctionStats, type BillingInfo, type Metrics, type UsageSummary } from "@/lib/api";
import { cn } from "@/lib/utils";

const ABSENT = "-";

/** Only a finite numeric API value is a measurement. A rejected poll, an older node, or a
 *  missing optional field stays absent ("-") instead of becoming an invented zero. */
function measured(v: number | null | undefined): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function sumMeasured<T>(rows: T[] | null | undefined, pick: (row: T) => number | null | undefined): number | null {
  if (!Array.isArray(rows)) return null;
  let total = 0;
  for (const row of rows) {
    const value = measured(pick(row));
    if (value === null) return null;
    total += value;
  }
  return total;
}

function fmtMeasured(n: number | null): string {
  const value = measured(n);
  return value === null ? ABSENT : fmt(value);
}

function unitMeasured(n: number | null, digits: number, unit: string): string {
  const value = measured(n);
  return value === null ? ABSENT : `${value.toFixed(digits)} ${unit}`;
}

function moneyMeasured(n: number | null): string {
  const value = measured(n);
  return value === null ? ABSENT : `$${value.toFixed(2)}`;
}

function chargeMeasured(units: number | null, cents: number | undefined): number | null {
  const u = measured(units);
  const c = measured(cents);
  return u === null || c === null ? null : (u * c) / 100;
}

function rowCharge(v: number | null): number {
  return measured(v) ?? Number.NaN;
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

  const requests = measured(metrics?.totals?.requests);
  const blocked = measured(metrics?.totals?.blocked);
  const cacheHits = sumMeasured(metrics?.series, (b) => b.cache_hits);
  const invocations = sumMeasured(fns, (f) => f.requests);
  const activeCpuMs = sumMeasured(fns, (f) => f.active_cpu_ms);
  const memGbHrs = sumMeasured(fns, (f) => f.memory_gb_hrs);
  const instances = sumMeasured(fns, (f) => f.instances);

  const rc = billing?.rate_card;
  const edgeCharge = chargeMeasured(requests === null ? null : requests / 1_000_000, rc?.requests_per_million_cents);
  const cpuCharge = chargeMeasured(activeCpuMs === null ? null : activeCpuMs / 1000 / 3600, rc?.active_cpu_hr_cents);
  const memCharge = chargeMeasured(memGbHrs, rc?.mem_gb_hr_cents);
  const wafCharge = chargeMeasured(blocked === null ? null : blocked / 1_000_000, rc?.waf_per_million_cents);
  const acc = billing?.account;
  const includedCents = measured(acc?.included_cents);
  const usedCents = measured(acc?.used_cents);
  const balanceCents = measured(acc?.balance_cents);
  const includedTotal = includedCents === null ? null : includedCents / 100;
  const includedUsed = usedCents === null ? null : usedCents / 100;
  const onDemand = balanceCents === null ? null : Math.max(0, -balanceCents) / 100;
  const creditBalance = balanceCents === null ? null : balanceCents / 100;
  const planName = acc?.plan ?? ABSENT;

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
  const chart = buckets.map((b, i) => ({
    date: bucketLabel(gran, b.t_ms),
    "Edge Requests": edgeS[i] ?? null,
    "Cache Hits": cacheS[i] ?? null,
    "Blocked": blockedS[i] ?? null,
  }));

  const includedPct = includedTotal !== null && includedTotal > 0 && includedUsed !== null
    ? Math.max(0, Math.min(100, (includedUsed / includedTotal) * 100))
    : null;

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
  const perM = (v?: number) => {
    const n = measured(v);
    return n === null ? null : n / 1_000_000;
  };
  const gb = (v?: number) => {
    const n = measured(v);
    return n === null ? null : n / GB;
  };
  const gbMonth = (v?: number) => {
    const n = measured(v);
    return n === null ? null : n / (GB * MS_MONTH);
  };
  const count = (v?: number) => {
    const n = measured(v);
    return n === null ? ABSENT : fmt(n);
  };
  const shown = (v: number | null, digits: number) => {
    const n = measured(v);
    return n === null ? ABSENT : n.toFixed(digits);
  };
  const price = (c: number | undefined, suffix: string) => {
    const n = measured(c);
    return n === null ? ABSENT : `$${(n / 100).toFixed(3)}${suffix}`;
  };
  const charge = (units: number | null, c?: number) => chargeMeasured(units, c);
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

  const heading = planName === ABSENT ? "Plan Usage" : `${planName} Plan Usage`;

  return (
    <div className="min-w-0 w-full max-w-full">
      <h1 className="mb-5 min-w-0 break-words text-2xl font-semibold capitalize tracking-tight">{heading}</h1>

      <div className="mb-4 grid grid-cols-1 gap-2 text-sm sm:grid-cols-2 lg:grid-cols-3">
        <Pill icon={<ChevronDown className="h-4 w-4 shrink-0" />}>Current Billing Cycle</Pill>
        <Pill icon={<Calendar className="h-4 w-4 shrink-0" />}>{billingRange()}</Pill>
        <Pill icon={<ChevronDown className="h-4 w-4 shrink-0" />}>All Products</Pill>
        <Pill icon={<ChevronDown className="h-4 w-4 shrink-0" />}>All Projects</Pill>
        <Pill icon={<ChevronDown className="h-4 w-4 shrink-0" />}>By Product</Pill>
        <button className="flex w-full max-w-full items-center justify-center rounded-md border border-border px-2 py-2 text-secondary hover:bg-subtle"><MoreHorizontal className="h-4 w-4 shrink-0" /></button>
      </div>

      <Card className="mb-4 min-w-0 w-full max-w-full">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          <div className="flex min-w-0 items-center gap-3">
            <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-blue-500/10 text-blue-500"><Coins className="h-4 w-4 shrink-0" /></span>
            <div className="min-w-0">
              <div className="text-sm text-secondary">Included Credit</div>
              <div className="min-w-0 break-words text-lg font-semibold tabular-nums whitespace-normal">
                {includedUsed === null || includedTotal === null ? ABSENT : `$${includedUsed.toFixed(2)} / $${includedTotal.toFixed(2)}`}
              </div>
            </div>
          </div>
          <div className="min-w-0 border-l border-border pl-6 sm:pl-6">
            <div className="text-sm text-secondary">Credit Balance</div>
            <div className={cn("min-w-0 break-words text-lg font-semibold tabular-nums whitespace-normal", creditBalance !== null && creditBalance < 0 && "text-red-500")}>
              {moneyMeasured(creditBalance)}
            </div>
          </div>
          <div className="flex min-w-0 items-center gap-3 sm:justify-end sm:text-right">
            <div className="min-w-0">
              <div className="text-sm text-secondary">On-Demand Charges</div>
              <div className="min-w-0 break-words text-lg font-semibold tabular-nums whitespace-normal">{moneyMeasured(onDemand)}</div>
            </div>
            <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-purple-500/10 text-purple-500"><DollarSign className="h-4 w-4 shrink-0" /></span>
          </div>
        </div>
        {includedPct === null ? (
          <div className="mt-4 min-w-0 break-words rounded-lg border border-dashed border-border px-4 py-3 text-sm text-secondary">
            Included credit usage is unavailable for this session.
          </div>
        ) : (
          <div className="mt-4 flex h-2.5 overflow-hidden rounded-full bg-subtle">
            <div className="h-full bg-[#0070f3]" style={{ width: `${includedPct}%` }} />
            <div className="h-full bg-[#7928ca]" style={{ width: `${100 - includedPct}%` }} />
          </div>
        )}
      </Card>

      <Card className="min-w-0 w-full max-w-full">
        <div className="mb-4 flex min-w-0 w-full max-w-full flex-wrap items-center justify-between gap-2">
          <h2 className="min-w-0 break-words text-lg font-semibold">Consumption Breakdown</h2>
          <div className="flex min-w-0 w-full max-w-full flex-wrap items-center gap-3 sm:gap-4">
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

        <div className="min-w-0 w-full max-w-full overflow-x-auto">
          <BarChart
            data={chart}
            index="date"
            categories={["Edge Requests", "Cache Hits", "Blocked"]}
            colors={["blue", "emerald", "rose"]}
            stack
            showLegend
            showAnimation
            valueFormatter={(v) => fmt(v)}
            className="h-72 w-full max-w-full"
          />
        </div>

        <div className="mt-6 min-w-0 w-full max-w-full overflow-x-auto">
          <div className="hidden grid-cols-1 gap-4 border-b border-border pb-2 text-xs font-medium uppercase tracking-wide text-muted sm:grid sm:grid-cols-3">
            <span>Product</span><span className="text-right">Usage</span><span className="text-right">Charge</span>
          </div>

          <GroupHeader name="OpenEdge Delivery Network" />
          <Row color="bg-blue-500" name="Edge Requests" spark={edgeS} usage={fmtMeasured(requests)} charge={rowCharge(edgeCharge)} />
          <Row color="bg-emerald-500" name="Cache Hits" spark={cacheS} usage={fmtMeasured(cacheHits)} charge={Number.NaN} />
          <Row color="bg-rose-500" name="Firewall — Blocked Requests" spark={blockedS} usage={fmtMeasured(blocked)} charge={rowCharge(wafCharge)} />

          <GroupHeader name="OpenEdge Functions (Fluid · Active CPU pricing)" />
          <Row color="bg-amber-500" name="Function Invocations" spark={[]} usage={fmtMeasured(invocations)} charge={Number.NaN} />
          <Row color="bg-purple-500" name="Active CPU" spark={[]} usage={unitMeasured(activeCpuMs === null ? null : activeCpuMs / 1000, 1, "s")} charge={rowCharge(cpuCharge)} />
          <Row color="bg-teal-500" name="Provisioned Memory" spark={[]} usage={unitMeasured(memGbHrs, 3, "GB-hr")} charge={rowCharge(memCharge)} />
          <Row color="bg-cyan-500" name="Provisioned Instances" spark={[]} usage={fmtMeasured(instances)} charge={Number.NaN} />
        </div>
      </Card>

      <Card className="mt-4 min-w-0 w-full max-w-full">
        <h2 className="min-w-0 break-words text-lg font-semibold">Storage &amp; network</h2>
        <p className="mb-4 mt-1 min-w-0 break-words text-sm text-secondary">
          Five metered SKUs from your usage summary. Rates are the public rate card; these sit apart from the compute totals above.
        </p>
        {!summaryReady || summaryError ? (
          <div className="min-w-0 break-words rounded-lg border border-dashed border-border px-4 py-8 text-center text-sm text-secondary">
            {summaryError
              ? "Usage summary is unavailable for this session. Sign in to see storage and network usage."
              : "No storage or network usage reported for this billing cycle."}
          </div>
        ) : (
          <div className="min-w-0 w-full max-w-full overflow-x-auto">
            <div className="hidden grid-cols-1 gap-2 border-b border-border pb-2 text-xs font-medium uppercase tracking-wide text-muted sm:grid sm:grid-cols-2 sm:gap-4 lg:grid-cols-4">
              <span>SKU</span><span className="text-right">Usage</span><span className="text-right">Rate</span><span className="text-right">Charge</span>
            </div>
            {storageRows.map((r) => (
              <div key={r.key} className="grid grid-cols-1 items-center gap-2 border-b border-border py-3 text-sm sm:grid-cols-2 sm:gap-4 lg:grid-cols-4">
                <span className="min-w-0 w-full max-w-full">
                  <span className="block break-words text-fg">{r.name}</span>
                  <span className="block break-words text-xs text-secondary">{r.unit}</span>
                </span>
                <span className="flex min-w-0 w-full max-w-full items-center justify-between gap-2 text-right tabular-nums text-secondary sm:block">
                  <span className="text-xs text-secondary sm:hidden">Usage</span>
                  <span className="min-w-0 break-words">{r.usage}</span>
                </span>
                <span className="flex min-w-0 w-full max-w-full items-center justify-between gap-2 text-right tabular-nums text-secondary sm:block">
                  <span className="text-xs text-secondary sm:hidden">Rate</span>
                  <span className="min-w-0 break-words">{r.rate}</span>
                </span>
                <span className="flex min-w-0 w-full max-w-full items-center justify-between gap-2 text-right tabular-nums sm:block">
                  <span className="text-xs text-secondary sm:hidden">Charge</span>
                  <span className="min-w-0 break-words">{moneyMeasured(r.amount)}</span>
                </span>
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
    <button className="flex min-w-0 w-full max-w-full items-center gap-2 rounded-md border border-border bg-card px-3 py-2 text-left text-secondary hover:bg-subtle">
      {icon}
      <span className="min-w-0 break-words text-fg">{children}</span>
    </button>
  );
}

function GroupHeader({ name }: { name: string }) {
  return <div className="min-w-0 w-full max-w-full break-words border-b border-border bg-subtle/60 px-1 py-2 text-sm font-medium text-secondary">{name}</div>;
}

function Row({ color, name, spark, usage, charge }: { color: string; name: string; spark: number[]; usage: string; charge: number }) {
  const data = spark.map((v, i) => ({ i, v }));
  return (
    <div className="grid grid-cols-1 items-center gap-2 border-b border-border py-3 sm:grid-cols-3 sm:gap-4">
      <span className="flex min-w-0 w-full max-w-full items-center gap-2.5 text-sm">
        <span className={cn("h-2.5 w-2.5 shrink-0 rounded-full", color)} />
        <span className="min-w-0 break-words">{name}</span>
      </span>
      <span className="flex min-w-0 w-full max-w-full items-center justify-between gap-3 sm:justify-end">
        <span className="text-xs text-secondary sm:hidden">Usage</span>
        {spark.length ? (
          <SparkAreaChart data={data} index="i" categories={["v"]} colors={["slate"]} className="hidden h-7 w-full max-w-full sm:block" />
        ) : null}
        <span className="min-w-0 w-full max-w-full break-words text-right text-sm tabular-nums text-secondary whitespace-normal">{usage}</span>
      </span>
      <span className="flex min-w-0 w-full max-w-full items-center justify-between gap-3 text-sm tabular-nums sm:justify-end">
        <span className="text-xs text-secondary sm:hidden">Charge</span>
        <span className="min-w-0 break-words text-right whitespace-normal">{moneyMeasured(measured(charge))}</span>
      </span>
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
