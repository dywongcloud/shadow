"use client";

import { Card, Badge, Table, Th, Td } from "@/components/ui";
import { usePoll, type Event } from "@/lib/api";

interface AuditEvent extends Event {
  project: string;
}

const tone: Record<string, "green" | "red" | "amber" | "blue" | "default"> = {
  deploy: "green",
  "domain-add": "blue",
  cron: "blue",
  "waf-deny": "red",
  throttled: "amber",
  redirect: "default",
  rewrite: "default",
};

export default function AuditPage() {
  const { data: events } = usePoll<AuditEvent[]>("/v1/admin/audit", 4000);

  return (
    <div>
      <div className="mb-6">
        <h1 className="text-2xl font-semibold tracking-tight">Audit Log</h1>
        <p className="mt-1 text-sm text-secondary">Control-plane and security-relevant activity across the platform.</p>
      </div>

      <Table>
        <thead><tr><Th>Time</Th><Th>Action</Th><Th>Project</Th><Th>Region</Th><Th>Target</Th><Th>Detail</Th></tr></thead>
        <tbody>
          {(events ?? []).map((e, i) => (
            <tr key={i}>
              <Td className="whitespace-nowrap text-secondary">{new Date(e.ts_ms).toLocaleString()}</Td>
              <Td><Badge tone={tone[e.action] ?? "default"}>{e.action}</Badge></Td>
              <Td className="text-secondary">{e.project || "—"}</Td>
              <Td><Badge tone="blue">{e.region}</Badge></Td>
              <Td className="font-mono text-xs text-secondary">{e.host}{e.path}</Td>
              <Td className="max-w-xs truncate text-secondary">{e.detail}</Td>
            </tr>
          ))}
        </tbody>
      </Table>
      {!events?.length && <Card className="mt-4 py-12 text-center text-sm text-secondary">No audit events yet.</Card>}
    </div>
  );
}
