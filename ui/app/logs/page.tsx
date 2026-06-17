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

export default function LogsPage() {
  const { data: events } = usePoll<Event[]>("/v1/logs?limit=200", 1500);
  return (
    <div>
      <PageHeader title="Observability" desc="Live edge request log (WAF / bot / cache / cron decisions)" />
      <Table>
        <thead>
          <tr><Th>When</Th><Th>Region</Th><Th>Method</Th><Th>Host</Th><Th>Path</Th><Th>Status</Th><Th>Action</Th><Th>Detail</Th></tr>
        </thead>
        <tbody>
          {(events ?? []).map((e, i) => (
            <tr key={i}>
              <Td className="text-muted whitespace-nowrap">{timeAgo(e.ts_ms)}</Td>
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
  );
}
