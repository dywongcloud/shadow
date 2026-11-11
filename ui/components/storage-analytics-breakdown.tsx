"use client";

import { useState } from "react";
import { Badge, Button, Card } from "@/components/ui";

export interface StorageBreakdownRow {
  id: string;
  name: string;
  kind: string;
  requests: number | null;
  share: number | null;
  bytesIn: number | null;
  bytesOut: number | null;
  reachability: string | null;
  measured: boolean | null;
  color: string;
}

export interface StorageEventRow {
  id: string;
  t_ms: number;
  dbName: string;
  dbColor: string;
  ok: boolean | null;
  latency_ms: number | null;
  bytes_in: number | null;
  bytes_out: number | null;
  error: string | null;
}

type SortKey = "name" | "kind" | "requests" | "share" | "bytesIn" | "bytesOut" | "reachability";
const MAX_EVENTS = 60;

function sortValue(row: StorageBreakdownRow, key: SortKey): string | number | null {
  if (key === "name") return row.name;
  if (key === "kind") return row.kind;
  if (key === "requests") return row.requests;
  if (key === "share") return row.share;
  if (key === "bytesIn") return row.bytesIn;
  if (key === "bytesOut") return row.bytesOut;
  return row.reachability;
}

function utcStamp(ms: number): string {
  return new Date(ms).toISOString().replace("T", " ").slice(0, 19) + " UTC";
}

/** Per-database request routing. Only columns a returned field backs: a missing counter is an
 *  absence, never a zero, and the share bar is measured against the sum of the reported counters. */
export function StorageAnalyticsBreakdown({
  rows,
  formatCount,
  formatBytes,
}: {
  rows: StorageBreakdownRow[];
  formatCount: (value: number) => string;
  formatBytes: (value: number) => string;
}) {
  const [sortKey, setSortKey] = useState<SortKey>("requests");
  const [ascending, setAscending] = useState(false);
  const sorted = rows.slice().sort((a, b) => {
    const left = sortValue(a, sortKey);
    const right = sortValue(b, sortKey);
    if (left === null && right === null) return 0;
    if (left === null) return 1;
    if (right === null) return -1;
    const comparison =
      typeof left === "number" && typeof right === "number"
        ? left - right
        : String(left).localeCompare(String(right));
    return ascending ? comparison : -comparison;
  });

  function toggleSort(key: SortKey) {
    if (sortKey === key) setAscending((value) => !value);
    else {
      setSortKey(key);
      setAscending(key === "name" || key === "kind" || key === "reachability");
    }
  }

  function header(key: SortKey, label: string) {
    return (
      <th
        scope="col"
        aria-sort={sortKey === key ? (ascending ? "ascending" : "descending") : "none"}
        className="min-w-0 border-b border-border p-0 text-left"
      >
        <Button
          variant="ghost"
          className="min-h-[44px] min-w-0 w-full justify-start gap-1 px-3 text-xs font-medium uppercase tracking-wide text-muted"
          aria-label={"Sort by " + label}
          onClick={() => toggleSort(key)}
        >
          <span className="min-w-0 break-words">{label}</span>
          <span aria-hidden="true" className="shrink-0 text-secondary">
            {sortKey === key ? (ascending ? "^" : "v") : "-"}
          </span>
        </Button>
      </th>
    );
  }

  return (
    <Card className="min-w-0 w-full max-w-full p-0">
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 border-b border-border px-4 py-3 sm:px-5">
        <h2 className="min-w-0 break-words text-sm font-semibold text-fg">Request routing by database</h2>
        <Badge tone="default" className="shrink-0 tabular-nums">{rows.length + " databases"}</Badge>
      </div>
      <p className="min-w-0 break-words px-4 pt-3 text-xs text-secondary sm:px-5">
        Counters reported by each database&apos;s metrics endpoint. Share is each database&apos;s requests divided by the sum of the reported request counters.
      </p>
      <div className="mt-3 min-w-0 w-full max-w-full overflow-x-auto">
        <table className="w-full min-w-[760px] border-collapse text-sm">
          <thead>
            <tr>
              {header("name", "Name")}
              {header("kind", "Kind")}
              {header("requests", "Requests")}
              {header("share", "Share")}
              {header("bytesIn", "Ingress")}
              {header("bytesOut", "Egress")}
              {header("reachability", "Reachability")}
            </tr>
          </thead>
          <tbody>
            {sorted.map((row) => (
              <tr key={row.id} className="border-b border-border align-top last:border-b-0">
                <th scope="row" className="min-w-0 max-w-[220px] p-3 text-left font-medium text-fg">
                  <span className="flex min-w-0 items-center gap-2">
                    <span className="h-2 w-2 shrink-0 rounded-full" style={{ background: row.color }} aria-hidden="true" />
                    <span className="min-w-0 break-words">{row.name}</span>
                  </span>
                </th>
                <td className="min-w-0 break-words p-3 text-secondary">{row.kind}</td>
                <td className="min-w-0 break-words p-3 tabular-nums text-fg">
                  {row.requests === null ? "—" : formatCount(row.requests)}
                </td>
                <td className="min-w-0 p-3">
                  {row.share === null ? (
                    <span className="text-muted">—</span>
                  ) : (
                    <span className="flex min-w-0 items-center gap-2">
                      <span
                        className="relative block h-2 min-w-0 flex-1 overflow-hidden rounded-full bg-subtle"
                        role="meter"
                        aria-label={row.name + " request share"}
                        aria-valuemin={0}
                        aria-valuemax={1}
                        aria-valuenow={row.share}
                        aria-valuetext={(row.share * 100).toFixed(1) + "% of reported requests"}
                      >
                        <span
                          className="absolute inset-y-0 left-0 rounded-full"
                          style={{
                            width: Math.max(0, Math.min(100, row.share * 100)) + "%",
                            background: "linear-gradient(90deg, " + row.color + "55, " + row.color + ")",
                          }}
                        />
                      </span>
                      <span className="shrink-0 text-xs tabular-nums text-secondary">
                        {(row.share * 100).toFixed(1)}%
                      </span>
                    </span>
                  )}
                </td>
                <td className="min-w-0 break-words p-3 tabular-nums text-fg">
                  {row.bytesIn === null ? "—" : formatBytes(row.bytesIn)}
                </td>
                <td className="min-w-0 break-words p-3 tabular-nums text-fg">
                  {row.bytesOut === null ? "—" : formatBytes(row.bytesOut)}
                </td>
                <td className="min-w-0 p-3">
                  {row.reachability === null && row.measured === null ? (
                    <span className="text-muted">—</span>
                  ) : (
                    <Badge tone={row.measured === true ? "green" : row.measured === false ? "amber" : "default"}>
                      <span className="min-w-0 break-words">{row.reachability ?? "not reported"}</span>
                    </Badge>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {rows.length === 0 ? (
        <div className="flex min-h-[88px] min-w-0 items-center gap-3 p-4 sm:p-5">
          <span
            className="h-12 w-1.5 shrink-0 rounded-full"
            style={{ background: "linear-gradient(180deg, #6366f1, #34d399)" }}
            aria-hidden="true"
          />
          <p className="min-w-0 break-words text-sm text-secondary">No databases are returned for this tenant.</p>
        </div>
      ) : null}
    </Card>
  );
}

/** The real `events` arrays, merged and sorted most-recent-first. An event prints only the fields
 *  the node actually returned; the error text is the fixed class the metrics endpoint reports. */
export function StorageAnalyticsEvents({
  events,
  formatBytes,
}: {
  events: StorageEventRow[];
  formatBytes: (value: number) => string;
}) {
  const shown = events.slice(0, MAX_EVENTS);
  return (
    <Card className="min-w-0 w-full max-w-full p-0">
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 border-b border-border px-4 py-3 sm:px-5">
        <h2 className="min-w-0 break-words text-sm font-semibold text-fg">Recent events</h2>
        <Badge tone="purple" className="shrink-0 tabular-nums">{events.length + " returned"}</Badge>
      </div>
      <p className="min-w-0 break-words px-4 pt-3 text-xs text-secondary sm:px-5">
        Request observations returned by the metrics endpoints, newest first. Timestamps are UTC.
      </p>
      {events.length === 0 ? (
        <div className="flex min-h-[88px] min-w-0 items-center gap-3 p-4 sm:p-5">
          <span
            className="h-12 w-1.5 shrink-0 rounded-full"
            style={{ background: "linear-gradient(180deg, #fbbf24, #fb7185)" }}
            aria-hidden="true"
          />
          <p className="min-w-0 break-words text-sm text-secondary">
            No request events are returned. A node that predates the events payload reports counters only.
          </p>
        </div>
      ) : (
        <>
          <div className="mt-3 max-h-80 min-w-0 w-full max-w-full overflow-y-auto border-t border-border">
            <ul className="min-w-0 divide-y divide-border">
              {shown.map((event) => {
                const hasMeasurements =
                  event.latency_ms !== null ||
                  event.bytes_in !== null ||
                  event.bytes_out !== null ||
                  event.error !== null;
                return (
                  <li key={event.id} className="min-w-0 p-3 sm:px-5">
                    <div className="flex min-w-0 flex-wrap items-center gap-2">
                      <span className="h-2 w-2 shrink-0 rounded-full" style={{ background: event.dbColor }} aria-hidden="true" />
                      <span className="min-w-0 break-words text-sm text-fg">{event.dbName}</span>
                      <Badge
                        tone={event.ok === true ? "green" : event.ok === false ? "red" : "default"}
                        className="shrink-0"
                      >
                        {event.ok === true ? "ok" : event.ok === false ? "error" : "outcome not reported"}
                      </Badge>
                      <span className="min-w-0 break-words text-xs tabular-nums text-muted">{utcStamp(event.t_ms)}</span>
                    </div>
                    <div className="mt-1.5 flex min-w-0 flex-wrap gap-x-3 gap-y-1 text-xs text-secondary">
                      {event.latency_ms !== null ? <span className="tabular-nums">Latency {event.latency_ms} ms</span> : null}
                      {event.bytes_in !== null ? <span className="tabular-nums">Ingress {formatBytes(event.bytes_in)}</span> : null}
                      {event.bytes_out !== null ? <span className="tabular-nums">Egress {formatBytes(event.bytes_out)}</span> : null}
                      {event.error !== null ? (
                        <span className="min-w-0 break-words">Error class: {event.error.replace(/_/g, " ")}</span>
                      ) : null}
                      {!hasMeasurements ? <span>No per-event measurements were returned.</span> : null}
                    </div>
                  </li>
                );
              })}
            </ul>
          </div>
          {events.length > shown.length ? (
            <p className="min-w-0 break-words border-t border-border px-4 py-3 text-xs text-secondary sm:px-5">
              Showing the {shown.length} most recent of {events.length} returned events.
            </p>
          ) : null}
        </>
      )}
    </Card>
  );
}
