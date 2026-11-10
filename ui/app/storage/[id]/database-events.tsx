"use client";

import { Info, ScrollText } from "lucide-react";
import { Badge, Card, StateNotice } from "@/components/ui";
import { usePoll, type Event } from "@/lib/api";

const POLL_MS = 15000;
const LIMIT = 100;

/** Server-derived timestamps are formatted as UTC so a server render and a client render agree
 *  (React #418). A malformed timestamp is stated as absent, never as epoch zero. */
function absoluteTime(ms?: number): string {
  if (typeof ms !== "number" || !Number.isFinite(ms) || ms <= 0) return "-";
  return `${new Date(ms).toISOString().slice(0, 19).replace("T", " ")} UTC`;
}

function tone(status: number): "green" | "amber" | "red" {
  if (status >= 500) return "red";
  if (status >= 400) return "amber";
  return "green";
}

/** The backend `/v1/logs` filter has NO database-id dimension: `Event` carries no database field
 *  and DB-gateway requests are tallied into `db_metrics`, not the event log. The only honest
 *  per-database view is a TENANT-WIDE, bounded search for events whose path or detail mentions
 *  this database id, using the real wire params `route=` (path substring) and `q=` (free text),
 *  both bounded at parse time. It must never be presented as the database's own query log. */
export function DatabaseEvents({ dbId }: { dbId: string }) {
  const query = `/v1/logs?limit=${LIMIT}&route=${encodeURIComponent("/v1/databases")}&q=${encodeURIComponent(dbId)}`;
  const { data, error } = usePoll<Event[]>(query, POLL_MS);
  const events = Array.isArray(data) ? data.slice(0, LIMIT) : null;

  return (
    <Card className="min-w-0 w-full max-w-full">
      <div className="mb-3 flex min-w-0 flex-wrap items-center gap-2">
        <ScrollText className="h-4 w-4 shrink-0 text-secondary" />
        <h3 className="text-sm font-semibold">Related platform events</h3>
        {events && <Badge tone="default">{events.length} of {LIMIT} max</Badge>}
      </div>

      <p className="mb-3 flex min-w-0 items-start gap-1.5 text-xs text-muted">
        <Info className="mt-0.5 h-3 w-3 shrink-0" />
        <span className="min-w-0 break-words">
          Tenant-wide events mentioning this database id. The backend has no per-database event
          dimension, so this is not a query log: database gateway traffic is counted in the
          metrics counters above. Only your tenant&apos;s events are returned.
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
        <ul className="m-0 flex min-w-0 list-none flex-col gap-2 p-0">
          {events.map((e, i) => (
            <li
              key={`${e.request_id ?? "event"}-${e.ts_ms ?? 0}-${i}`}
              className="min-w-0 rounded-lg border border-border p-3 text-sm"
            >
              <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
                <span className="shrink-0 font-mono text-xs tabular-nums text-muted">{absoluteTime(e.ts_ms)}</span>
                <span className="shrink-0 font-mono text-xs text-secondary">{e.method || "-"}</span>
                {typeof e.status === "number" && <Badge tone={tone(e.status)}>{e.status}</Badge>}
                {e.region && <span className="min-w-0 break-all text-xs text-muted">{e.region}</span>}
              </div>
              <div className="mt-1 min-w-0 break-all font-mono text-xs text-fg">{e.path || "-"}</div>
              {e.host && <div className="mt-1 min-w-0 break-all text-xs text-muted">{e.host}</div>}
              {e.detail && <div className="mt-1 min-w-0 break-words text-xs text-secondary">{e.detail}</div>}
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

export const DATABASE_EVENTS_POLL_MS = POLL_MS;
