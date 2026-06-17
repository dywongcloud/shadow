"use client";

import { Server, Globe2 } from "lucide-react";
import { Card, Badge, Table, Th, Td } from "@/components/ui";
import { usePoll, type NodeInfo, type AdminOverview } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function AdminNodesPage() {
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 3000);
  const { data: ov } = usePoll<AdminOverview>("/v1/admin/overview", 4000);
  const regions = Array.from(new Set((nodes ?? []).map((n) => n.region))).sort();

  return (
    <div>
      <div className="mb-6">
        <h1 className="text-2xl font-semibold tracking-tight">Infrastructure</h1>
        <p className="mt-1 text-sm text-secondary">Nodes meshed over iroh P2P, coordinated by a leader-elected cluster.</p>
      </div>

      <div className="mb-6 grid grid-cols-2 gap-3 sm:grid-cols-4">
        <Card className="flex flex-col gap-1 p-4">
          <span className="flex items-center gap-1.5 text-[11px] uppercase tracking-wide text-muted"><Server className="h-3.5 w-3.5" />Nodes</span>
          <span className="text-2xl font-semibold">{nodes?.length ?? 0}</span>
        </Card>
        <Card className="flex flex-col gap-1 p-4">
          <span className="flex items-center gap-1.5 text-[11px] uppercase tracking-wide text-muted"><Globe2 className="h-3.5 w-3.5" />Regions</span>
          <span className="text-2xl font-semibold">{regions.length}</span>
        </Card>
        <Card className="flex flex-col gap-1 p-4">
          <span className="text-[11px] uppercase tracking-wide text-muted">Leader</span>
          <span className="truncate font-mono text-sm">{ov?.cluster?.leader ?? "—"}</span>
        </Card>
        <Card className="flex flex-col gap-1 p-4">
          <span className="text-[11px] uppercase tracking-wide text-muted">Term</span>
          <span className="text-2xl font-semibold">{ov?.cluster?.term ?? "—"}</span>
        </Card>
      </div>

      <Table>
        <thead><tr><Th>Node</Th><Th>Region</Th><Th>Endpoint</Th><Th>Role</Th><Th>Last seen</Th></tr></thead>
        <tbody>
          {(nodes ?? []).map((n) => (
            <tr key={n.id}>
              <Td className="font-medium">{n.name}</Td>
              <Td><Badge tone="blue">{n.region}</Badge></Td>
              <Td className="font-mono text-xs text-secondary">{n.public_url}</Td>
              <Td>{n.id === ov?.cluster?.leader ? <Badge tone="amber">leader</Badge> : n.is_self ? <Badge tone="green">this node</Badge> : <Badge>peer</Badge>}</Td>
              <Td className="text-secondary">{timeAgo(n.last_seen_ms)} ago</Td>
            </tr>
          ))}
        </tbody>
      </Table>
    </div>
  );
}
