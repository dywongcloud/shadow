"use client";

import { useState } from "react";
import { AreaChart, BarList, DonutChart } from "@tremor/react";
import { Activity, AlertTriangle, ShieldX, Gauge } from "lucide-react";
import { Card, PageHeader, Stat } from "@/components/ui";
import { usePoll, type Metrics } from "@/lib/api";

const WINDOWS = [
  { m: 30, label: "30m" },
  { m: 60, label: "1h" },
  { m: 180, label: "3h" },
];

export default function MonitoringPage() {
  const [win, setWin] = useState(60);
  const { data } = usePoll<Metrics>(`/v1/metrics?minutes=${win}`, 4000);

  const series = (data?.series ?? []).map((b) => ({
    time: new Date(b.t_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
    Requests: b.requests,
    Errors: b.errors + b.client_err,
    Blocked: b.blocked,
  }));

  const cache = (data?.series ?? []).map((b) => ({
    time: new Date(b.t_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
    Hits: b.cache_hits,
    Misses: b.cache_miss,
  }));

  const statusData = Object.entries(data?.status_distribution ?? {})
    .sort()
    .map(([name, value]) => ({ name, value }));
  const topPaths = (data?.top_paths ?? []).map((p) => ({ name: p.path, value: p.count }));

  const t = data?.totals;

  return (
    <div>
      <PageHeader
        title="Monitoring"
        desc="Live request analytics across the edge — traffic, errors, firewall blocks and CDN cache."
        action={
          <div className="flex items-center gap-1 rounded-md border border-border p-0.5">
            {WINDOWS.map((w) => (
              <button
                key={w.m}
                onClick={() => setWin(w.m)}
                className={`rounded px-2.5 py-1 text-xs font-medium ${win === w.m ? "bg-subtle text-fg" : "text-secondary hover:text-fg"}`}
              >
                {w.label}
              </button>
            ))}
          </div>
        }
      />

      <div className="mb-6 grid grid-cols-2 gap-4 lg:grid-cols-4">
        <Stat label="Requests" value={(t?.requests ?? 0).toLocaleString()} hint={<span className="flex items-center gap-1"><Activity className="h-3 w-3" /> last {win}m</span>} />
        <Stat label="Error rate" value={`${((t?.error_rate ?? 0) * 100).toFixed(1)}%`} hint={<span className="flex items-center gap-1"><AlertTriangle className="h-3 w-3" /> 4xx + 5xx</span>} />
        <Stat label="Blocked" value={(t?.blocked ?? 0).toLocaleString()} hint={<span className="flex items-center gap-1"><ShieldX className="h-3 w-3" /> WAF + bots</span>} />
        <Stat label="Cache hit ratio" value={`${((t?.cache_hit_ratio ?? 0) * 100).toFixed(0)}%`} hint={<span className="flex items-center gap-1"><Gauge className="h-3 w-3" /> CDN</span>} />
      </div>

      <Card className="mb-6">
        <h3 className="mb-1 text-sm font-semibold">Requests over time</h3>
        <p className="mb-4 text-xs text-secondary">Per-minute traffic with errors and firewall blocks overlaid.</p>
        <AreaChart
          className="h-72"
          data={series}
          index="time"
          categories={["Requests", "Errors", "Blocked"]}
          colors={["blue", "rose", "amber"]}
          showLegend
          showGridLines={false}
          curveType="monotone"
          yAxisWidth={40}
        />
      </Card>

      <div className="mb-6 grid grid-cols-1 gap-4 lg:grid-cols-3">
        <Card>
          <h3 className="mb-4 text-sm font-semibold">Status codes</h3>
          {statusData.length ? (
            <DonutChart
              className="h-52"
              data={statusData}
              category="value"
              index="name"
              colors={["emerald", "blue", "amber", "rose"]}
              showLabel
            />
          ) : (
            <Empty />
          )}
        </Card>
        <Card className="lg:col-span-2">
          <h3 className="mb-4 text-sm font-semibold">Top paths</h3>
          {topPaths.length ? <BarList data={topPaths} color="blue" /> : <Empty />}
        </Card>
      </div>

      <Card>
        <h3 className="mb-1 text-sm font-semibold">CDN cache</h3>
        <p className="mb-4 text-xs text-secondary">Hits served from the edge vs. origin misses.</p>
        <AreaChart
          className="h-56"
          data={cache}
          index="time"
          categories={["Hits", "Misses"]}
          colors={["emerald", "slate"]}
          showLegend
          showGridLines={false}
          curveType="monotone"
          yAxisWidth={40}
          stack
        />
      </Card>
    </div>
  );
}

function Empty() {
  return <div className="flex h-52 items-center justify-center text-sm text-secondary">No data yet</div>;
}
