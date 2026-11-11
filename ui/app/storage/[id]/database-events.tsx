"use client";

import { useState } from "react";
import { Info, ScrollText } from "lucide-react";
import { Badge, Card, StateNotice } from "@/components/ui";
import { usePoll, type Event } from "@/lib/api";

const POLL_MS = 15000;
const LIMIT = 100;

/** Server-derived timestamps are formatted as UTC so a server render and a client render agree
 *  (React #418). A malformed timestamp is stated as absent, never as epoch zero. */
function absoluteTime(ms?: number): string {
  if (typeof ms !== "number" || !Number.isFinite(ms) || ms <= 0) return "time not reported";
  return `${new Date(ms).toISOString().slice(0, 19).replace("T", " ")} UTC`;
}

function tone(status: number): "green" | "amber" | "red" {
  if (status >= 500) return "red";
  if (status >= 400) return "amber";
  return "green";
}

/** A SECONDARY, collapsed view. The backend `/v1/logs` filter has NO database-id dimension:
 *  `Event` carries no database field and DB-gateway requests are tallied into `db_metrics`, not
 *  the event log. The only honest per-database view is a TENANT-WIDE, bounded search for events
 *  whose path or detail mentions this database id, using the real wire params `route=` (path
 *  substring) and `q=` (free text), both bounded at parse time. This is NOT a query log: the
 *  database's measured request log is the primary panel above it. */
export function DatabaseEvents({ dbId }: { dbId: string }) {
  const [open, setOpen] = useState(false);
  const query = `/v1/logs?limit=${LIMIT}&route=${encodeURIComponent("/v1/databases")}&q=${encodeURIComponent(dbId)}`;
  const { data, error } = usePoll<Event[]>(query, POLL_MS, open);
  const events = Array.isArray(data) ? data.slice(0, LIMIT) : null;

  return (
    <details
      className="min-w-0 w-full max-w-full rounded-xl border border-border bg-card"
      onToggle={(e) => setOpen(e.currentTarget.open)}
    >
      <summary className="flex min-h-[44px] min-w-0 w-full max-w-full cursor-pointer flex-wrap items-center gap-2 px-4 py-3 text-sm marker:content-none sm:px-5">
        <ScrollText className="h-4 w-4 shrink-0 text-secondary" aria-hidden="true" />
        <span className="min-w-0 break-words font-medium text-fg">Tenant-wide platform events (secondary)</span>
        <Badge tone="default" className="shrink-0">not a request log</Badge>
        <span className="min-w-0 break-words text-xs text-muted">Expand to see platform events mentioning this database id.</span>
      </summary>

      <div className="min-w-0 border-t border-border px-4 py-3 sm:px-5">
        <p className="mb-3 flex min-w-0 items-start gap-1.5 text-xs text-muted">
          <Info className="mt-0.5 h-3 w-3 shrink-0" />
          <span className="min-w-0 break-words">
            Tenant-wide events mentioning this database id. The backend has no per-database event
            dimension, so this is not a query log: database gateway traffic is counted in the
            measured request log and the metrics counters. Only your tenant&apos;s events are returned.
          </span>
        </p>

        {error && (
          <StateNotice>Couldn&apos;t load events: {String(error).replace(/^Error:\s*/, "")}</StateNotice>
        )}

        {!error && !events && <p className="text-xs text-muted">Loading…</p>}

        {events && events.length === 0 && (
          <p className="text-xs text-muted">No platform events mention this database in the current window.</p>
        )}

        {events && events.length > 0 && (
          <>
            <div className="mb-2 min-w-0 break-words text-xs tabular-nums text-muted">{events.length} of {LIMIT} max</div>
            <div className="min-w-0 w-full max-w-full overflow-x-auto">
              <table className="w-full min-w-[640px] border-collapse text-xs">
                <thead>
                  <tr className="border-b border-border text-muted">
                    <th scope="col" className="min-w-0 p-2 text-left font-medium uppercase tracking-wide">Time (UTC)</th>
                    <th scope="col" className="min-w-0 p-2 text-left font-medium uppercase tracking-wide">Request</th>
                    <th scope="col" className="min-w-0 p-2 text-left font-medium uppercase tracking-wide">Outcome</th>
                  </tr>
                </thead>
                <tbody>
                  {events.map((e, i) => (
                    <tr key={`${e.request_id ?? "event"}-${e.ts_ms ?? 0}-${i}`} className="border-b border-border align-top">
                      <td className="min-w-0 whitespace-nowrap p-3 font-mono tabular-nums text-muted">{absoluteTime(e.ts_ms)}</td>
                      <td className="min-w-0 p-3">
                        <span className="mr-2 font-mono text-secondary">{e.method || "-"}</span>
                        <span className="min-w-0 break-all font-mono text-fg">{e.path || "-"}</span>
                        {e.host && <span className="mt-1 block min-w-0 break-all text-muted">{e.host}</span>}
                        {e.detail && <span className="mt-1 block min-w-0 break-words text-secondary">{e.detail}</span>}
                      </td>
                      <td className="min-w-0 p-3">
                        {typeof e.status === "number" && <Badge tone={tone(e.status)}>{e.status}</Badge>}
                        {e.region && <span className="mt-1 block min-w-0 break-all text-muted">{e.region}</span>}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </>
        )}
      </div>
    </details>
  );
}

export const DATABASE_EVENTS_POLL_MS = POLL_MS;
