"use client";

import { Activity, Info } from "lucide-react";
import { Badge, Card, Stat } from "@/components/ui";
import { apiGet, usePoll, type DbInstanceMetrics } from "@/lib/api";
import type { Database } from "@/lib/api";

const POLL_MS = 20000;

/** Human labels for the per-kind gauges the backend measures. Unknown keys fall through to a
 *  title-cased key, because a node newer than this dashboard can add one. */
const LABELS: Record<string, string> = {
  bytes: "Size",
  file_bytes: "File size",
  objects: "Objects",
  depth: "Depth",
  vectors: "Vectors",
  dimensions: "Dimensions",
  subscribers: "Subscribers",
  published: "Published",
  tables: "Tables",
  backends: "Connections",
  xact_commit: "Commits",
  xact_rollback: "Rollbacks",
  tup_inserted: "Rows inserted",
  tup_updated: "Rows updated",
  tup_deleted: "Rows deleted",
  tup_returned: "Rows read",
  blks_hit: "Cache hits",
  blks_read: "Disk reads",
  deadlocks: "Deadlocks",
  used_memory: "Memory",
  used_memory_peak: "Peak memory",
  connected_clients: "Clients",
  total_commands_processed: "Commands",
  instantaneous_ops_per_sec: "Ops/sec",
  keys: "Keys (keyspace)",
  dbsize: "Keys",
  expires: "Volatile keys",
  expired_keys: "Expired",
  evicted_keys: "Evicted",
  uptime_in_seconds: "Uptime",
  open_streams: "Open streams",
};

/** Keys that name a size or a duration, so the value reads as one. */
const BYTE_KEYS = /bytes|memory|maxmemory/;

function humanBytes(n: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

function humanUptime(sec: number): string {
  if (sec < 60) return `${sec}s`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m`;
  if (sec < 86400) return `${Math.floor(sec / 3600)}h ${Math.floor((sec % 3600) / 60)}m`;
  return `${Math.floor(sec / 86400)}d ${Math.floor((sec % 86400) / 3600)}h`;
}

function fmt(key: string, v: unknown): string {
  if (typeof v === "boolean") return v ? "yes" : "no";
  if (typeof v !== "number") return String(v);
  if (key === "uptime_in_seconds") return humanUptime(v);
  if (BYTE_KEYS.test(key)) return humanBytes(v);
  return v.toLocaleString();
}

function label(key: string): string {
  return LABELS[key] ?? key.replace(/_/g, " ").replace(/^./, (c) => c.toUpperCase());
}

/** Per-INSTANCE metrics for one database: the gauges this kind actually has, plus how the number was
 *  obtained. A value the node did not measure renders as an absence with a reason, never as a zero. */
export function InstanceMetrics({ db }: { db: Database }) {
  const { data, error } = usePoll<DbInstanceMetrics>(`/v1/databases/${db.id}/metrics`, POLL_MS);
  const stats = data?.stats ?? null;
  const entries = Object.entries(stats ?? {}).filter(([, v]) => typeof v !== "object");
  const reach = data?.reachability ?? "";
  const measured = data?.measured === true;

  return (
    <Card>
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <Activity className="h-4 w-4 text-secondary" />
        <h3 className="text-sm font-semibold">Instance metrics</h3>
        {measured ? (
          <Badge tone="green">measured</Badge>
        ) : (
          <Badge tone="amber">{reach ? reach.replace(/_/g, " ") : "not measured"}</Badge>
        )}
        {data?.served_by && <Badge tone="default">via {data.served_by}</Badge>}
        {data?.host_node && <span className="text-xs text-muted">host {data.host_node}</span>}
      </div>

      {error && (
        <p className="text-xs text-red-500">
          Couldn&apos;t load metrics: {String(error).replace(/^Error:\s*/, "")}
        </p>
      )}

      {!error && !data && <p className="text-xs text-muted">Loading…</p>}

      {data && entries.length === 0 ? (
        <p className="text-xs text-muted">{data.note || "No gauge is available for this instance."}</p>
      ) : (
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          {entries.slice(0, 8).map(([k, v]) => (
            <Stat key={k} label={label(k)} value={fmt(k, v)} />
          ))}
        </div>
      )}

      {data?.note && entries.length > 0 && (
        <p className="mt-3 flex items-start gap-1.5 text-xs text-muted">
          <Info className="mt-0.5 h-3 w-3 shrink-0" />
          {data.note}
        </p>
      )}
      {data?.counters && typeof data.counters.requests === "number" && data.counters.requests > 0 && (
        <p className="mt-3 text-xs text-muted">
          {data.counters.requests.toLocaleString()} gateway requests
          {typeof data.counters.errors === "number" && data.counters.errors > 0
            ? ` · ${data.counters.errors} errors`
            : ""}
          {typeof data.counters.last_latency_ms === "number"
            ? ` · last ${data.counters.last_latency_ms} ms`
            : ""}
        </p>
      )}
    </Card>
  );
}

/** Server-side prefetch is not used here: the read is tenant-scoped and node-addressed, so it runs
 *  from the browser like every other poller on this page. `apiGet` stays exported so a future
 *  prefetch uses the same tenant-scoped cache key rather than a second fetch path. */
export const INSTANCE_METRICS_POLL_MS = POLL_MS;
export { apiGet };
