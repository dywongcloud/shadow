"use client";

import { Server } from "lucide-react";
import { Badge, Card } from "@/components/ui";
import type { DbInstanceMetrics } from "@/lib/api";
import { ABSENT, DB_COLORS, dbBytes, dbFinite, dbPlain, dbUtc } from "./db-chart";

/** `series_sources` (or the older `sources`) names the nodes that contributed to the measured
 *  series. The counters on the wire are ONE aggregate for the whole database: the response does
 *  NOT split requests or bytes per node. A single contributor therefore owns every measured
 *  counter; with several, per-node cells stay absent and only the real total is printed. */
function sourcesOf(data: DbInstanceMetrics): { names: string[]; field: string } {
  const series = Array.isArray(data.series_sources) ? data.series_sources : null;
  const legacy = Array.isArray(data.sources) ? data.sources : null;
  const list = series ?? legacy ?? [];
  const names = Array.from(new Set(list.filter((n): n is string => typeof n === "string" && n.trim().length > 0).map((n) => n.trim())));
  return { names, field: series ? "series_sources" : legacy ? "sources" : "not reported" };
}

/** A colored share bar is only drawn for a share that is actually measured: 100% of the one
 *  aggregate counter set. No per-node fraction is estimated from a node count. */
function ShareBar({ pct, color }: { pct: number; color: string }) {
  return (
    <span
      className="db-fill relative block h-2 min-w-0 w-full overflow-hidden rounded-full bg-subtle"
      role="meter"
      aria-label="Share of measured requests"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={pct}
      aria-valuetext={`${pct}% of measured requests`}
    >
      <span
        className="absolute inset-y-0 left-0 rounded-full"
        style={{ width: `${pct}%`, background: `linear-gradient(90deg, ${color}, transparent)` }}
      />
    </span>
  );
}

export function DbRouting({ data, error }: { data: DbInstanceMetrics | null; error: string | null }) {
  const counters = data?.counters;
  const requests = dbFinite(counters?.requests);
  const errors = dbFinite(counters?.errors);
  const ingress = dbFinite(counters?.bytes_in);
  const egress = dbFinite(counters?.bytes_out);
  const lastMs = dbFinite(counters?.last_ms);
  const observedMs = dbFinite(data?.observed_at_ms);
  const { names, field } = data ? sourcesOf(data) : { names: [], field: "not reported" };
  const oneNode = names.length === 1;
  const hasCounters = requests !== null || ingress !== null || egress !== null;

  return (
    <Card className="min-w-0 w-full max-w-full p-0" aria-label="Request routing">
      <div className="flex min-w-0 flex-wrap items-center gap-2 border-b border-border px-4 py-3 sm:px-5">
        <Server className="h-4 w-4 shrink-0 text-secondary" aria-hidden="true" />
        <h3 className="min-w-0 break-words text-sm font-semibold text-fg">Request routing</h3>
        <Badge tone={oneNode ? "green" : "default"} className="shrink-0 tabular-nums">
          {dbPlain(names.length)} {names.length === 1 ? "node" : "nodes"}
        </Badge>
      </div>

      <div className="flex min-w-0 flex-col gap-1.5 border-b border-border px-4 py-3 sm:px-5">
        <p className="min-w-0 break-words text-xs text-secondary">
          {oneNode
            ? `One node contributes to this series (${names[0]}), so the measured counters below are its own total.`
            : names.length > 1
              ? `The response names ${names.length} contributing nodes but reports counters only in aggregate; per-node splits are not measured.`
              : "No contributing node name is reported in this metrics response."}
        </p>
        {!oneNode && names.length > 1 && (
          <p className="min-w-0 break-words text-xs text-muted">
            Node names come from <code className="font-mono">{field}</code>. A share bar is shown only for the measured total, never estimated from a node count.
          </p>
        )}
        <p className="min-w-0 break-words text-xs text-muted">
          Last seen is the timestamp of the last measured request in the counters, not a node heartbeat.
          {data?.host_node ? ` Host node: ${data.host_node}.` : ""}
          {data?.node ? ` Answering node: ${data.node}.` : ""}
          {data?.served_by ? ` Proxied via ${data.served_by}.` : ""}
        </p>
      </div>

      {error && <p className="min-w-0 break-words px-4 py-3 text-sm text-red-500 sm:px-5">Couldn&apos;t load metrics: {error.replace(/^Error:\s*/, "")}</p>}

      {names.length === 0 && !hasCounters ? (
        <div className="flex min-h-[120px] min-w-0 items-center gap-3 px-4 py-5 sm:px-5">
          <span
            className="db-fill h-12 w-1.5 shrink-0 rounded-full"
            style={{ background: "linear-gradient(180deg, var(--db-cyan-lit, #22d3ee), var(--db-indigo-lit, #6366f1))" }}
            aria-hidden="true"
          />
          <p className="min-w-0 break-words text-sm text-secondary">
            No routing rows are available: this response names no contributing node and reports no gateway counters.
          </p>
        </div>
      ) : (
        <div className="min-w-0 w-full max-w-full overflow-x-auto">
          <table className="w-full min-w-[720px] border-collapse text-xs">
            <thead>
              <tr className="border-b border-border bg-subtle/40 text-muted">
                {["Node", "Requests", "Share", "Ingress", "Egress", "Last seen"].map((label) => (
                  <th key={label} scope="col" className="min-w-0 whitespace-nowrap p-3 text-left font-medium uppercase tracking-wide">{label}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {names.map((name) => (
                <tr key={name} className="border-b border-border align-top">
                  <th scope="row" className="min-w-0 max-w-[220px] p-3 text-left font-mono font-medium text-fg">
                    <span className="flex min-w-0 items-center gap-2">
                      <span className="h-2 w-2 shrink-0 rounded-full" style={{ background: DB_COLORS.indigo.color }} aria-hidden="true" />
                      <span className="min-w-0 break-all">{name}</span>
                    </span>
                  </th>
                  <td className="min-w-0 p-3 tabular-nums">{oneNode ? dbPlain(requests) : ABSENT}</td>
                  <td className="min-w-0 w-[22%] p-3">
                    {oneNode && requests !== null && requests > 0 ? (
                      <ShareBar pct={100} color={DB_COLORS.indigo.color} />
                    ) : (
                      <span className="min-w-0 break-words text-muted">{oneNode ? ABSENT : "not split per node"}</span>
                    )}
                  </td>
                  <td className="min-w-0 p-3 tabular-nums">{oneNode ? dbBytes(ingress) : ABSENT}</td>
                  <td className="min-w-0 p-3 tabular-nums">{oneNode ? dbBytes(egress) : ABSENT}</td>
                  <td className="min-w-0 whitespace-nowrap p-3 tabular-nums">{oneNode ? dbUtc(lastMs) : ABSENT}</td>
                </tr>
              ))}
              <tr className="border-b border-border bg-subtle/30 align-top">
                <th scope="row" className="min-w-0 p-3 text-left font-medium text-fg">
                  <span className="flex min-w-0 items-center gap-2">
                    <span className="h-2 w-2 shrink-0 rounded-full" style={{ background: DB_COLORS.emerald.color }} aria-hidden="true" />
                    <span className="min-w-0 break-words">Measured total</span>
                  </span>
                </th>
                <td className="min-w-0 p-3 tabular-nums">{dbPlain(requests)}</td>
                <td className="min-w-0 w-[22%] p-3">
                  {requests !== null && requests > 0 ? (
                    <ShareBar pct={100} color={DB_COLORS.emerald.color} />
                  ) : (
                    <span className="min-w-0 break-words text-muted">{ABSENT}</span>
                  )}
                </td>
                <td className="min-w-0 p-3 tabular-nums">{dbBytes(ingress)}</td>
                <td className="min-w-0 p-3 tabular-nums">{dbBytes(egress)}</td>
                <td className="min-w-0 whitespace-nowrap p-3 tabular-nums">{dbUtc(lastMs ?? observedMs)}</td>
              </tr>
            </tbody>
          </table>
        </div>
      )}

      <div className="flex min-w-0 flex-col gap-1 border-t border-border px-4 py-3 sm:px-5">
        <p className="min-w-0 break-words text-xs text-muted">
          Columns for active connections, total connections, region and a world map are not shown: no field in this response measures them for a database.
        </p>
        {errors !== null && (
          <p className="min-w-0 break-words text-xs tabular-nums text-muted">
            {dbPlain(errors)} measured {errors === 1 ? "error" : "errors"} across {dbPlain(requests)} requests.
          </p>
        )}
      </div>
    </Card>
  );
}
