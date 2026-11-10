"use client";

import { Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { usePoll, type Event } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

function tone(a: string) {
  if (a.includes("deny") || a.includes("block") || a === "throttled") return "red" as const;
  if (a.includes("cache")) return "blue" as const;
  if (a === "cron" || a === "redirect" || a === "rewrite") return "amber" as const;
  return "green" as const;
}

/** Absolute, server-derived UTC time. A relative "ago" hint drifts under the
 *  1500 ms poll and cannot date an event; an invalid ts_ms renders absent. */
function absoluteUtc(ms: number): string {
  if (!Number.isFinite(ms)) return "";
  try {
    const iso = new Date(ms).toISOString();
    return `${iso.slice(0, 19).replace("T", " ")} UTC`;
  } catch {
    return "";
  }
}

function whenLabel(ms: number) {
  const abs = absoluteUtc(ms);
  if (!abs) return null;
  return <>{abs}<span className="ml-1.5 text-[10px]">({timeAgo(ms)})</span></>;
}

export default function LogsPage() {
  const { data: events } = usePoll<Event[]>("/v1/logs?limit=200", 1500);
  return (
    <div className="min-w-0">
      <PageHeader title="Observability" desc="Live edge request log (WAF / bot / cache / cron decisions)" />
      <div className="min-w-0 sm:hidden" role="list" aria-label="Edge request events">
        {(events ?? []).map((e, i) => (
          <div key={i} role="listitem" className="mb-2 min-w-0 rounded-xl border border-border bg-card p-3 last:mb-0">
            <dl className="min-w-0 space-y-1.5 text-xs">
              <div className="min-w-0">
                <dt className="text-[10px] uppercase tracking-wide text-muted">When</dt>
                <dd className="min-w-0 break-all text-muted" aria-label="When">{whenLabel(e.ts_ms)}</dd>
              </div>
              <div className="min-w-0">
                <dt className="text-[10px] uppercase tracking-wide text-muted">Region</dt>
                <dd className="min-w-0 break-all text-muted" aria-label="Region">{e.region}</dd>
              </div>
              <div className="min-w-0">
                <dt className="text-[10px] uppercase tracking-wide text-muted">Method / Status</dt>
                <dd className="min-w-0 break-all font-mono" aria-label="Method and status">{e.method} <span className="tabular-nums">{e.status}</span></dd>
              </div>
              <div className="min-w-0">
                <dt className="text-[10px] uppercase tracking-wide text-muted">Path</dt>
                <dd className="min-w-0 break-all font-mono" aria-label="Path">{e.path}</dd>
              </div>
              <div className="min-w-0">
                <dt className="text-[10px] uppercase tracking-wide text-muted">Host</dt>
                <dd className="min-w-0 break-all font-mono text-muted" aria-label="Host">{e.host}</dd>
              </div>
              <div className="min-w-0">
                <dt className="text-[10px] uppercase tracking-wide text-muted">Action</dt>
                <dd className="min-w-0 break-all" aria-label="Action">{e.action ? <Badge tone={tone(e.action)}>{e.action}</Badge> : null}</dd>
              </div>
              <div className="min-w-0">
                <dt className="text-[10px] uppercase tracking-wide text-muted">Detail</dt>
                <dd className="min-w-0 break-all text-muted" aria-label="Detail">{e.detail}</dd>
              </div>
            </dl>
          </div>
        ))}
        {!events?.length && <div role="listitem" className="min-w-0"><p className="min-w-0 rounded-xl border border-border bg-card p-4 text-sm text-muted">No events yet.</p></div>}
      </div>
      <div className="hidden min-w-0 max-w-full sm:block">
      <Table>
        <thead>
          <tr><Th>When</Th><Th>Region</Th><Th>Method</Th><Th>Host</Th><Th>Path</Th><Th>Status</Th><Th>Action</Th><Th>Detail</Th></tr>
        </thead>
        <tbody>
          {(events ?? []).map((e, i) => (
            <tr key={i}>
              <Td className="text-muted whitespace-nowrap">{whenLabel(e.ts_ms)}</Td>
              <Td className="text-muted">{e.region}</Td>
              <Td className="font-mono text-xs">{e.method}</Td>
              <Td className="font-mono text-xs text-muted">{e.host}</Td>
              <Td className="font-mono text-xs">{e.path}</Td>
              <Td className="tabular-nums">{e.status}</Td>
              <Td><Badge tone={tone(e.action)}>{e.action}</Badge></Td>
              <Td className="text-muted text-xs">{e.detail}</Td>
            </tr>
          ))}
          {!events?.length && <tr><Td className="text-muted">No events yet.</Td></tr>}
        </tbody>
      </Table>
      </div>
    </div>
  );
}
