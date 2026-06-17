"use client";

import { useState } from "react";
import { BarChart, SparkAreaChart } from "@tremor/react";
import { Calendar, ChevronDown, Coins, DollarSign, MoreHorizontal } from "lucide-react";
import { Card } from "@/components/ui";
import { usePoll, type Overview, type FunctionStats } from "@/lib/api";
import { cn } from "@/lib/utils";

const DAYS = 18;

// Build a plausible cumulative daily series for the billing cycle from current
// totals, so the consumption chart looks live without historical storage.
function series(total: number) {
  const out: number[] = [];
  let acc = 0;
  for (let i = 0; i < DAYS; i++) {
    acc += (total / DAYS) * (0.6 + (i / DAYS) * 0.9);
    out.push(Math.round(acc));
  }
  // normalize so the last point equals total
  const last = out[out.length - 1] || 1;
  return out.map((v) => Math.round((v / last) * total));
}

function fmt(n: number) {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(2) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1) + "K";
  return String(n);
}

export default function UsagePage() {
  const { data: ov } = usePoll<Overview>("/v1/overview", 3000);
  const { data: fns } = usePoll<FunctionStats[]>("/v1/functions", 3000);
  const [gran, setGran] = useState<"Daily" | "Weekly" | "Monthly">("Daily");
  const [cumulative, setCumulative] = useState(true);

  const requests = ov?.requests ?? 0;
  const blocked = ov?.blocked ?? 0;
  const cacheHits = ov?.cdn.hits ?? 0;
  const fluidMs = (fns ?? []).reduce((a, f) => a + f.fluid_ms, 0);
  const invocations = (fns ?? []).reduce((a, f) => a + f.requests, 0);

  // Pretend cost model.
  const edgeCharge = (requests / 1_000_000) * 2.0;
  const fnCharge = (fluidMs / 1000 / 3600) * 0.18;
  const wafCharge = (blocked / 1_000_000) * 0.6;
  const onDemand = edgeCharge + fnCharge + wafCharge;
  const includedUsed = Math.min(20, onDemand);

  // Stacked chart data.
  const edgeS = series(requests);
  const fnS = series(invocations * 30);
  const cacheS = series(cacheHits);
  const start = new Date();
  start.setDate(start.getDate() - DAYS + 1);
  const chart = Array.from({ length: DAYS }, (_, i) => {
    const d = new Date(start);
    d.setDate(start.getDate() + i);
    const label = d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
    return {
      date: label,
      "Edge Requests": cumulative ? edgeS[i] : Math.max(1, edgeS[i] - (edgeS[i - 1] || 0)),
      "Function Invocations": cumulative ? fnS[i] : Math.max(1, fnS[i] - (fnS[i - 1] || 0)),
      "Cache Hits": cumulative ? cacheS[i] : Math.max(0, cacheS[i] - (cacheS[i - 1] || 0)),
    };
  });

  const pct = Math.min(100, (includedUsed / 20) * 100);

  return (
    <div>
      <h1 className="mb-5 text-2xl font-semibold tracking-tight">{ov?.concurrency.plan === "enterprise" ? "Enterprise" : "Hobby"} Plan Usage</h1>

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
              <div className="text-lg font-semibold tabular-nums">${includedUsed.toFixed(2)} / $20.00</div>
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
          categories={["Edge Requests", "Function Invocations", "Cache Hits"]}
          colors={["blue", "amber", "emerald"]}
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

          <GroupHeader name="Hive Delivery Network" />
          <Row color="bg-blue-500" name="Edge Requests" spark={edgeS} usage={`${fmt(requests)} / 10M`} charge={edgeCharge} />
          <Row color="bg-emerald-500" name="Cache Hits" spark={cacheS} usage={`${fmt(cacheHits)}`} charge={0} />
          <Row color="bg-rose-500" name="Firewall — Blocked Requests" spark={series(blocked)} usage={`${fmt(blocked)}`} charge={wafCharge} />

          <GroupHeader name="Hive Functions" />
          <Row color="bg-amber-500" name="Function Invocations" spark={fnS} usage={`${fmt(invocations)}`} charge={0} />
          <Row color="bg-purple-500" name="Active CPU (Fluid)" spark={series(fluidMs)} usage={`${(fluidMs / 1000).toFixed(1)} s`} charge={fnCharge} />
          <Row color="bg-cyan-500" name="Provisioned Instances" spark={series((ov?.instances ?? 0) * 100)} usage={`${ov?.instances ?? 0}`} charge={0} />
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
