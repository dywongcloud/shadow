"use client";

import Link from "next/link";
import { AreaChart } from "@tremor/react";
import {
  Users, FolderGit2, Rocket, Database, Server, Activity, ShieldX, Siren, Boxes, Webhook,
} from "lucide-react";
import { Card, Badge } from "@/components/ui";
import { useOpsPoll, type AdminOverview, type Metrics, type Incident } from "@/lib/api";

export default function AdminOverviewPage() {
  const { data: ov } = useOpsPoll<AdminOverview>("/v1/admin/overview", 4000);
  const { data: metrics } = useOpsPoll<Metrics>("/v1/metrics?minutes=60", 5000);
  const { data: incidents } = useOpsPoll<Incident[]>("/v1/incidents", 6000);

  const series = (metrics?.series ?? []).map((b) => ({
    time: new Date(b.t_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
    Requests: b.requests,
    Errors: b.errors + b.client_err,
  }));
  const openIncidents = (incidents ?? []).filter((i) => i.status !== "resolved");
  const errRate = ov ? (ov.error_rate_30m * 100).toFixed(2) : "0";
  const healthy = ov ? ov.incidents_open === 0 && ov.error_rate_30m < 0.05 : true;

  return (
    <div>
      <div className="mb-6 flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Platform Overview</h1>
          <p className="mt-1 text-sm text-secondary">Owner: <span className="font-mono">{ov?.owner ?? "—"}</span></p>
        </div>
        <Badge tone={healthy ? "green" : "red"}>
          <span className={`h-2 w-2 rounded-full ${healthy ? "bg-green" : "bg-red-500"}`} />
          {healthy ? "All systems operational" : `${ov?.incidents_open} active incident(s)`}
        </Badge>
      </div>

      <div className="mb-6 grid grid-cols-2 gap-3 lg:grid-cols-4">
        <Tile icon={<Users className="h-4 w-4" />} label="Teams" value={ov?.teams} />
        <Tile icon={<FolderGit2 className="h-4 w-4" />} label="Projects" value={ov?.projects} />
        <Tile icon={<Rocket className="h-4 w-4" />} label="Deployments" value={ov?.deployments} />
        <Tile icon={<Database className="h-4 w-4" />} label="Databases" value={ov?.databases?.total} hint={`${ov?.databases?.live ?? 0} live`} />
        <Tile icon={<Server className="h-4 w-4" />} label="Nodes" value={ov?.nodes} hint={(ov?.regions ?? []).join(", ")} />
        <Tile icon={<Boxes className="h-4 w-4" />} label="Instances" value={ov?.instances} />
        <Tile icon={<Activity className="h-4 w-4" />} label="Requests" value={ov?.requests?.toLocaleString()} />
        <Tile icon={<ShieldX className="h-4 w-4" />} label="Blocked" value={ov?.blocked?.toLocaleString()} />
      </div>

      <div className="mb-6 grid grid-cols-1 gap-4 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <div className="mb-3 flex items-center justify-between">
            <h3 className="text-sm font-semibold">Platform traffic (1h)</h3>
            <Badge tone={ov && ov.error_rate_30m > 0.05 ? "red" : "green"}>err {errRate}% · 30m</Badge>
          </div>
          <AreaChart
            className="h-60"
            data={series}
            index="time"
            categories={["Requests", "Errors"]}
            colors={["blue", "rose"]}
            showLegend
            showGridLines={false}
            curveType="monotone"
            yAxisWidth={40}
          />
        </Card>

        <Card>
          <div className="mb-3 flex items-center gap-2 text-sm font-semibold"><Siren className="h-4 w-4" /> Active incidents</div>
          {openIncidents.length ? (
            <div className="space-y-2">
              {openIncidents.map((i) => (
                <Link key={i.id} href="/admin/incidents" className="block rounded-lg border border-border p-3 hover:bg-subtle">
                  <div className="flex items-center justify-between gap-2">
                    <span className="truncate text-sm font-medium">{i.title}</span>
                    <SeverityBadge s={i.severity} />
                  </div>
                  <div className="mt-1 text-xs capitalize text-secondary">{i.status}</div>
                </Link>
              ))}
            </div>
          ) : (
            <div className="flex h-40 flex-col items-center justify-center gap-2 text-center text-sm text-secondary">
              <span className="flex h-9 w-9 items-center justify-center rounded-full bg-green/10 text-green">✓</span>
              No active incidents
            </div>
          )}
        </Card>
      </div>

      <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
        <Card>
          <div className="mb-3 flex items-center gap-2 text-sm font-semibold"><Server className="h-4 w-4" /> Cluster</div>
          {ov?.cluster && (
            <div className="space-y-1.5 text-sm">
              <Row k="Consensus" v={ov.cluster.consensus} />
              <Row k="Leader" v={`${ov.cluster.leader}${ov.cluster.is_leader ? " (this node)" : ""}`} />
              <Row k="Term" v={String(ov.cluster.term)} />
              <Row k="Members" v={String(ov.cluster.members.length)} />
            </div>
          )}
        </Card>
        <Card>
          <div className="mb-3 flex items-center gap-2 text-sm font-semibold"><Webhook className="h-4 w-4" /> Integrations</div>
          <div className="space-y-1.5 text-sm">
            <Row k="Webhooks configured" v={String(ov?.webhooks ?? 0)} />
            <Row k="Regions" v={(ov?.regions ?? []).join(", ") || "—"} />
            <Row k="Live databases" v={String(ov?.databases?.live ?? 0)} />
          </div>
        </Card>
      </div>
    </div>
  );
}

function Tile({ icon, label, value, hint }: { icon: React.ReactNode; label: string; value: React.ReactNode; hint?: string }) {
  return (
    <Card className="flex flex-col gap-1 p-4">
      <span className="flex items-center gap-1.5 text-[11px] font-medium uppercase tracking-wide text-muted">{icon}{label}</span>
      <span className="text-2xl font-semibold tabular-nums text-fg">{value ?? "—"}</span>
      {hint ? <span className="truncate text-xs text-secondary">{hint}</span> : null}
    </Card>
  );
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex justify-between gap-2">
      <span className="text-muted">{k}</span>
      <span className="truncate font-mono text-xs text-fg">{v}</span>
    </div>
  );
}

function SeverityBadge({ s }: { s: string }) {
  const tone = s === "critical" ? "red" : s === "major" ? "amber" : "blue";
  return <Badge tone={tone as "red" | "amber" | "blue"}>{s}</Badge>;
}
