"use client";

import { Activity, Info, Radio } from "lucide-react";
import { Badge, Card, Stat, StateNotice } from "@/components/ui";
import { apiGet, type DbInstanceMetrics } from "@/lib/api";

/** ONE poll for the whole observability section lives in `database-detail-client.tsx`; every
 *  panel (gauges, charts, routing, quota cards, request log) reads that same response, so a
 *  chart and a counter can never disagree about which reading they are showing. */
export const INSTANCE_METRICS_POLL_MS = 20000;

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

/** Small-multiple groups, in the order a reader scans them. A key no group names is still
 *  measured and still shown, under "Other" — nothing the node reported is dropped. */
const GROUPS: { name: string; keys: string[] }[] = [
  {
    name: "Size & storage",
    keys: ["bytes", "file_bytes", "objects", "depth", "vectors", "dimensions", "tables", "dbsize", "keys", "expires", "used_memory", "used_memory_peak"],
  },
  {
    name: "Throughput & operations",
    keys: ["xact_commit", "xact_rollback", "tup_inserted", "tup_updated", "tup_deleted", "tup_returned", "blks_hit", "blks_read", "total_commands_processed", "instantaneous_ops_per_sec", "published", "open_streams", "uptime_in_seconds"],
  },
  {
    name: "Connections & clients",
    keys: ["backends", "connected_clients", "subscribers"],
  },
  {
    name: "Health",
    keys: ["deadlocks", "expired_keys", "evicted_keys"],
  },
];

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

/** Server-derived timestamps are formatted as UTC so a server render and a client render agree
 *  (React #418); a missing observation time is stated as absent, never as a client-side "now". */
function sampledAt(ms?: number): string {
  if (typeof ms !== "number" || !Number.isFinite(ms) || ms <= 0) return "observation time not reported";
  return `sampled ${new Date(ms).toISOString().slice(11, 19)} UTC`;
}

/** The counters are measured on a node. A fleet-merged payload names its contributors in
 *  `sources`; an owner-proxied payload names `served_by`; otherwise the reading is local to the
 *  answering node. No scope is ever guessed. */
function counterScope(data: DbInstanceMetrics): string {
  const sources = data.series_sources ?? data.sources;
  if (Array.isArray(sources) && sources.length > 1) return `fleet sum across ${sources.length} nodes`;
  if (data.served_by) return `owner reading via ${data.served_by}`;
  const node = data.node || data.host_node;
  return node ? `this node's local slice (${node})` : "scope not reported";
}

const count = (v?: number) =>
  typeof v === "number" && Number.isFinite(v) ? v.toLocaleString() : null;

/** Per-INSTANCE metrics for one database: every gauge this kind actually has, grouped, plus
 *  when it was sampled and where the counters came from. A value the node did not measure
 *  renders as an absence with a reason, never as a zero. */
export function InstanceMetrics({ data, error }: { data: DbInstanceMetrics | null; error: string | null }) {
  const stats = data?.stats ?? null;
  const entries = Object.entries(stats ?? {}).filter(([, v]) => typeof v !== "object");
  const reach = data?.reachability ?? "";
  const measured = data?.measured === true;
  const scope = data ? counterScope(data) : "scope not reported";

  const used = new Set<string>();
  const grouped: { name: string; entries: [string, unknown][] }[] = [];
  for (const g of GROUPS) {
    const groupEntries = entries.filter(([k]) => g.keys.includes(k) && !used.has(k));
    for (const [k] of groupEntries) used.add(k);
    if (groupEntries.length > 0) grouped.push({ name: g.name, entries: groupEntries });
  }
  const other = entries.filter(([k]) => !used.has(k));
  if (other.length) grouped.push({ name: "Other", entries: other });

  const counters = data?.counters;
  const counterRows: { key: string; label: string; value: string | null }[] = [
    { key: "requests", label: "Gateway requests", value: count(counters?.requests) },
    { key: "errors", label: "Gateway errors", value: count(counters?.errors) },
    { key: "bytes_in", label: "Ingress", value: typeof counters?.bytes_in === "number" && Number.isFinite(counters.bytes_in) ? humanBytes(counters.bytes_in) : null },
    { key: "bytes_out", label: "Egress", value: typeof counters?.bytes_out === "number" && Number.isFinite(counters.bytes_out) ? humanBytes(counters.bytes_out) : null },
    { key: "last_latency_ms", label: "Last latency", value: typeof counters?.last_latency_ms === "number" && Number.isFinite(counters.last_latency_ms) ? `${counters.last_latency_ms} ms` : null },
    { key: "error_rate", label: "Error rate", value: typeof counters?.error_rate === "number" && Number.isFinite(counters.error_rate) ? `${(counters.error_rate * 100).toFixed(1)}%` : null },
  ].filter((r) => r.value !== null);

  return (
    <Card className="min-w-0 w-full max-w-full">
      <div className="mb-3 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1.5">
        <Activity className="h-4 w-4 shrink-0 text-secondary" />
        <h3 className="text-sm font-semibold">Instance gauges</h3>
        {measured ? (
          <Badge tone="green">measured</Badge>
        ) : (
          <Badge tone="amber">{reach ? reach.replace(/_/g, " ") : "not measured"}</Badge>
        )}
        {data?.served_by && <Badge tone="default">via {data.served_by}</Badge>}
        {data?.host_node && <span className="text-xs text-muted">host {data.host_node}</span>}
        {data && (
          <span className="flex min-w-0 items-center gap-1 text-xs text-muted">
            <Radio className="h-3 w-3 shrink-0" />
            {sampledAt(data.observed_at_ms)} · every {INSTANCE_METRICS_POLL_MS / 1000} s
          </span>
        )}
      </div>

      {error && (
        <StateNotice>Couldn&apos;t load metrics: {error.replace(/^Error:\s*/, "")}</StateNotice>
      )}

      {!error && !data && <p className="text-xs text-muted">Loading…</p>}

      {data && entries.length === 0 ? (
        <p className="min-w-0 break-words text-xs text-muted">{data.note || "No gauge is available for this instance."}</p>
      ) : (
        <div className="flex min-w-0 flex-col gap-4">
          {grouped.map((g) => (
            <section key={g.name} className="min-w-0" aria-label={g.name}>
              <h4 className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">{g.name}</h4>
              <div className="grid grid-cols-1 gap-3 min-[380px]:grid-cols-2 lg:grid-cols-4">
                {g.entries.map(([k, v]) => (
                  <Stat key={k} label={label(k)} value={fmt(k, v)} />
                ))}
              </div>
            </section>
          ))}
        </div>
      )}

      {data?.note && entries.length > 0 && (
        <p className="mt-3 flex min-w-0 items-start gap-1.5 text-xs text-muted">
          <Info className="mt-0.5 h-3 w-3 shrink-0" />
          <span className="min-w-0 break-words">{data.note}</span>
        </p>
      )}

      {counterRows.length > 0 && (
        <section className="mt-4 min-w-0" aria-label="Gateway counters">
          <h4 className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">Gateway counters</h4>
          <p className="mb-2 min-w-0 break-words text-xs text-muted">
            {scope}. Server-derived from this database&apos;s measured counters.
          </p>
          <div className="grid grid-cols-1 gap-3 min-[380px]:grid-cols-2 lg:grid-cols-4">
            {counterRows.map((r) => (
              <Stat key={r.key} label={r.label} value={r.value as string} hint={scope} />
            ))}
          </div>
        </section>
      )}
    </Card>
  );
}

/** `apiGet` stays exported so a future prefetch uses the same tenant-scoped cache key rather
 *  than a second fetch path. */
export { apiGet };
