"use client";

import { useMemo, useState } from "react";
import { ListFilter, ScrollText } from "lucide-react";
import { Badge, Button, Card, Input, StateNotice } from "@/components/ui";
import type { DbMetricEvent } from "@/lib/api";
import { DB_COLORS, dbBytes, dbFinite, dbMs, dbPlain, dbUtc } from "./db-chart";

const PAGE = 25;
const MAX_EVENTS = 200;

type SortKey = "t_ms" | "ok" | "latency_ms" | "bytes_in" | "bytes_out";
type Outcome = "all" | "ok" | "error";

/** The server's fixed error classes (`db_metrics::ErrorClass::label`). A DSN or engine message is
 *  never echoed into a dashboard; only these class names arrive on the wire. Unknown labels are
 *  shown verbatim rather than guessed at. */
function errorLabel(raw: string): string {
  const known: Record<string, string> = {
    engine_unreachable: "engine unreachable",
    bad_request: "bad request",
    query_error: "query error",
    timeout: "timeout",
    denied: "denied",
  };
  return known[raw] ?? (raw || "error class not reported");
}

function cleanEvents(raw: unknown): { events: DbMetricEvent[]; omitted: number } {
  const list = Array.isArray(raw) ? raw.slice(0, MAX_EVENTS) : [];
  const events: DbMetricEvent[] = [];
  let omitted = 0;
  for (const item of list) {
    if (!item || typeof item !== "object") {
      omitted += 1;
      continue;
    }
    const e = item as Record<string, unknown>;
    const fields = ["t_ms", "latency_ms", "bytes_in", "bytes_out"];
    const values = fields.map((k) => dbFinite(e[k]));
    if (values.some((v) => v === null) || (values[0] as number) <= 0 || typeof e.ok !== "boolean") {
      omitted += 1;
      continue;
    }
    events.push({
      t_ms: values[0] as number,
      ok: e.ok,
      latency_ms: values[1] as number,
      bytes_in: values[2] as number,
      bytes_out: values[3] as number,
      error: typeof e.error === "string" ? e.error : "",
    });
  }
  return { events, omitted };
}

const COLUMNS: { key: SortKey; label: string }[] = [
  { key: "t_ms", label: "Time (UTC)" },
  { key: "ok", label: "Outcome" },
  { key: "latency_ms", label: "Latency" },
  { key: "bytes_in", label: "Bytes in" },
  { key: "bytes_out", label: "Bytes out" },
];

/** The REAL per-database request log: `events` from `GET /v1/databases/:id/metrics`, most recent
 *  first, at most 200. This is a gateway request log, not the tenant-wide platform event search.
 *  Every filter and sort runs over rows the node actually returned; none are synthesized. */
export function DbLogTable({
  events: raw,
  available,
  error,
}: {
  events: Partial<DbMetricEvent>[] | null | undefined;
  available: boolean;
  error: string | null;
}) {
  const [outcome, setOutcome] = useState<Outcome>("all");
  const [minLatency, setMinLatency] = useState("");
  const [search, setSearch] = useState("");
  const [sortKey, setSortKey] = useState<SortKey>("t_ms");
  const [ascending, setAscending] = useState(false);
  const [limit, setLimit] = useState(PAGE);

  const { events, omitted } = useMemo(() => cleanEvents(raw), [raw]);
  const reported = Array.isArray(raw);

  const filtered = useMemo(() => {
    const floor = minLatency.trim() === "" ? null : Number(minLatency);
    const boundedFloor = floor !== null && Number.isFinite(floor) && floor >= 0 ? floor : null;
    const needle = search.trim().toLowerCase();
    const rows = events.filter((e) => {
      if (outcome === "ok" && !e.ok) return false;
      if (outcome === "error" && e.ok) return false;
      if (boundedFloor !== null && e.latency_ms < boundedFloor) return false;
      if (needle && !e.error.toLowerCase().includes(needle)) return false;
      return true;
    });
    rows.sort((a, b) => {
      let cmp = 0;
      if (sortKey === "ok") cmp = Number(a.ok) - Number(b.ok);
      else cmp = a[sortKey] - b[sortKey];
      if (cmp === 0) cmp = a.t_ms - b.t_ms;
      return ascending ? cmp : -cmp;
    });
    return rows;
  }, [events, outcome, minLatency, search, sortKey, ascending]);

  const shown = filtered.slice(0, Math.min(limit, MAX_EVENTS));

  function toggleSort(key: SortKey) {
    if (key === sortKey) setAscending((v) => !v);
    else {
      setSortKey(key);
      setAscending(false);
    }
    setLimit(PAGE);
  }

  return (
    <Card className="min-w-0 w-full max-w-full p-0" aria-label="Database request log">
      <div className="flex min-w-0 flex-wrap items-center gap-2 border-b border-border px-4 py-3 sm:px-5">
        <ScrollText className="h-4 w-4 shrink-0 text-secondary" aria-hidden="true" />
        <h3 className="min-w-0 break-words text-sm font-semibold text-fg">Request log</h3>
        {reported && (
          <Badge tone="default" className="shrink-0 tabular-nums">
            {dbPlain(events.length)} of {MAX_EVENTS} max
          </Badge>
        )}
      </div>

      <div className="flex min-w-0 flex-col gap-2 border-b border-border px-4 py-3 sm:px-5">
        <p className="min-w-0 break-words text-xs text-secondary">
          Requests measured at this database&apos;s gateway, newest first. Only the most recent {MAX_EVENTS} are returned by the node; times are UTC as reported by the server.
        </p>
        <div role="group" aria-label="Filter by outcome" className="flex min-w-0 flex-wrap gap-2">
          <ListFilter className="mt-3 h-4 w-4 shrink-0 text-muted" aria-hidden="true" />
          {(["all", "ok", "error"] as const).map((key) => {
            const isActive = outcome === key;
            return (
              <Button
                key={key}
                variant={isActive ? "default" : "outline"}
                aria-pressed={isActive}
                className="min-h-[44px] min-w-0 px-3 text-xs"
                style={isActive
                  ? { background: "linear-gradient(90deg, var(--db-indigo-lit, #6366f1), var(--db-cyan-lit, #22d3ee))", color: "#ffffff", borderColor: "transparent" }
                  : undefined}
                onClick={() => { setOutcome(key); setLimit(PAGE); }}
              >
                {key === "all" ? "All" : key === "ok" ? "Successful" : "Errors"}
              </Button>
            );
          })}
          <label className="flex min-w-0 flex-1 basis-40 items-center gap-2 text-xs text-secondary">
            <span className="shrink-0">Min latency (ms)</span>
            <Input
              type="number"
              min={0}
              step={1}
              inputMode="numeric"
              value={minLatency}
              onChange={(e) => { setMinLatency(e.target.value); setLimit(PAGE); }}
              className="min-h-[44px] min-w-0 flex-1"
              aria-label="Minimum latency in milliseconds"
            />
          </label>
          <label className="flex min-w-0 flex-1 basis-48 items-center gap-2 text-xs text-secondary">
            <span className="shrink-0">Error class</span>
            <Input
              type="search"
              value={search}
              onChange={(e) => { setSearch(e.target.value); setLimit(PAGE); }}
              placeholder="Search error label"
              className="min-h-[44px] min-w-0 flex-1"
              aria-label="Search the error class label"
            />
          </label>
        </div>
      </div>

      {error && <div className="px-4 pt-3 sm:px-5"><StateNotice>Couldn&apos;t load metrics: {error.replace(/^Error:\s*/, "")}</StateNotice></div>}

      {!reported && !error ? (
        <div className="flex min-h-[140px] min-w-0 items-center gap-3 px-4 py-5 sm:px-5">
          <span
            className="db-fill h-12 w-1.5 shrink-0 rounded-full"
            style={{ background: "linear-gradient(180deg, var(--db-rose-lit, #fb7185), var(--db-violet-lit, #a78bfa))" }}
            aria-hidden="true"
          />
          <p className="min-w-0 break-words text-sm text-secondary">
            {available
              ? "This metrics response has no request log. Nodes on an older build report counters only; the measured totals are on the Overview tab."
              : "Waiting for the metrics response before a request log can be shown."}
          </p>
        </div>
      ) : reported && events.length === 0 ? (
        <p className="min-w-0 break-words px-4 py-5 text-sm text-secondary sm:px-5">
          No requests are recorded for this database in the reported window.
        </p>
      ) : reported && filtered.length === 0 ? (
        <p className="min-w-0 break-words px-4 py-5 text-sm text-secondary sm:px-5">
          No recorded requests match these filters. Clear a filter to see the measured rows.
        </p>
      ) : reported ? (
        <>
          <div className="min-w-0 w-full max-w-full overflow-x-auto">
            <table className="w-full min-w-[640px] border-collapse text-xs">
              <thead>
                <tr className="border-b border-border bg-subtle/40 text-muted">
                  {COLUMNS.map((col) => {
                    const isSorted = sortKey === col.key;
                    return (
                      <th key={col.key} scope="col" aria-sort={isSorted ? (ascending ? "ascending" : "descending") : "none"} className="min-w-0 p-2 text-left">
                        <Button
                          variant="ghost"
                          className="min-h-[44px] min-w-0 justify-start gap-1 px-2 text-xs"
                          aria-label={`Sort by ${col.label}`}
                          onClick={() => toggleSort(col.key)}
                        >
                          <span className="min-w-0 break-words">{col.label}</span>
                          <span aria-hidden="true" className="shrink-0 text-muted">{isSorted ? (ascending ? "^" : "v") : "-"}</span>
                        </Button>
                      </th>
                    );
                  })}
                </tr>
              </thead>
              <tbody>
                {shown.map((e, i) => (
                  <tr key={`${e.t_ms}-${i}`} className="border-b border-border align-top">
                    <td className="min-w-0 whitespace-nowrap p-3 font-mono tabular-nums text-fg">{dbUtc(e.t_ms)}</td>
                    <td className="min-w-0 p-3">
                      {e.ok ? (
                        <Badge tone="green">ok</Badge>
                      ) : (
                        <Badge tone="red" className="min-w-0 max-w-full">
                          <span className="min-w-0 break-words">{errorLabel(e.error)}</span>
                        </Badge>
                      )}
                    </td>
                    <td className="min-w-0 whitespace-nowrap p-3 tabular-nums">{dbMs(e.latency_ms)}</td>
                    <td className="min-w-0 whitespace-nowrap p-3 tabular-nums">{dbBytes(e.bytes_in)}</td>
                    <td className="min-w-0 whitespace-nowrap p-3 tabular-nums">{dbBytes(e.bytes_out)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
            <span className="min-w-0 break-words text-xs tabular-nums text-muted">
              Showing {dbPlain(shown.length)} of {dbPlain(filtered.length)} matching · sorted by {COLUMNS.find((c) => c.key === sortKey)?.label ?? "time"} {ascending ? "ascending" : "descending"}
            </span>
            {shown.length < filtered.length && (
              <Button variant="outline" className="min-h-[44px] min-w-0 text-xs" onClick={() => setLimit((v) => Math.min(MAX_EVENTS, v + PAGE))}>
                Show more
              </Button>
            )}
          </div>
        </>
      ) : null}

      {omitted > 0 && (
        <p className="min-w-0 break-words border-t border-border px-4 py-3 text-xs text-muted sm:px-5">
          {dbPlain(omitted)} returned {omitted === 1 ? "row was" : "rows were"} incomplete and are not shown; no field was filled in.
        </p>
      )}

      <div className="flex min-w-0 items-center gap-2 border-t border-border px-4 py-3 sm:px-5">
        <span className="h-0.5 w-5 shrink-0 rounded-full" style={{ background: DB_COLORS.rose.color }} aria-hidden="true" />
        <p className="min-w-0 break-words text-xs text-muted">
          An error badge carries the server&apos;s fixed error class, never an engine message or connection string.
        </p>
      </div>
    </Card>
  );
}
