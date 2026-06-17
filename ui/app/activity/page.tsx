"use client";

import { Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { usePoll, type Event } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

function tone(a: string) {
  if (a.includes("deny") || a.includes("block") || a === "throttled") return "red" as const;
  if (a.includes("cache")) return "blue" as const;
  if (a === "cron" || a === "redirect" || a === "rewrite" || a === "deploy") return "amber" as const;
  return "green" as const;
}

export default function ActivityPage() {
  const { data: events } = usePoll<Event[]>("/v1/logs?limit=200", 1500);
  return (
    <div>
      <PageHeader title="Activity" desc="Every request, deploy, and edge decision across the cloud" />
      <Table>
        <thead>
          <tr><Th>When</Th><Th>Region</Th><Th>Method</Th><Th>Host</Th><Th>Path</Th><Th>Status</Th><Th>Action</Th><Th>Detail</Th></tr>
        </thead>
        <tbody>
          {(events ?? []).map((e, i) => (
            <tr key={i}>
              <Td className="whitespace-nowrap text-secondary">{timeAgo(e.ts_ms)} ago</Td>
              <Td className="text-secondary">{e.region}</Td>
              <Td className="font-mono text-xs">{e.method}</Td>
              <Td className="font-mono text-xs text-secondary">{e.host}</Td>
              <Td className="font-mono text-xs">{e.path}</Td>
              <Td className="tabular-nums">{e.status || "—"}</Td>
              <Td><Badge tone={tone(e.action)}>{e.action}</Badge></Td>
              <Td className="text-xs text-secondary">{e.detail}</Td>
            </tr>
          ))}
          {!events?.length && <tr><Td className="text-secondary">No activity yet.</Td></tr>}
        </tbody>
      </Table>
    </div>
  );
}
