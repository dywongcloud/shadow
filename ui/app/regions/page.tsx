"use client";

import { Card, Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { usePoll, type NodeInfo } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function RegionsPage() {
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 3000);
  const regions = Array.from(new Set((nodes ?? []).map((n) => n.region))).sort();

  return (
    <div>
      <PageHeader title="Regions" desc="Your MacBooks, meshed into one cloud" />
      <div className="mb-5 flex flex-wrap gap-2">
        {regions.map((r) => (
          <Badge key={r} tone="blue">◍ {r}</Badge>
        ))}
        {!regions.length && <span className="text-sm text-muted">discovering…</span>}
      </div>
      <Table>
        <thead><tr><Th>Node</Th><Th>Region</Th><Th>URL</Th><Th>Role</Th><Th>Seen</Th></tr></thead>
        <tbody>
          {(nodes ?? []).map((n) => (
            <tr key={n.id}>
              <Td className="font-medium">{n.name}</Td>
              <Td><Badge tone="blue">{n.region}</Badge></Td>
              <Td className="font-mono text-xs text-muted">{n.public_url}</Td>
              <Td>{n.is_self ? <Badge tone="green">this node</Badge> : <Badge>peer</Badge>}</Td>
              <Td className="text-muted">{timeAgo(n.last_seen_ms)}</Td>
            </tr>
          ))}
          {!nodes?.length && <tr><Td className="text-muted">No nodes.</Td></tr>}
        </tbody>
      </Table>
      <Card className="mt-4 text-sm text-muted">
        Add a node: run <code className="text-fg">hive-cloud --region iad1 --name node-b --peer http://&lt;this-mac-ip&gt;:8786</code> on another MacBook.
      </Card>
    </div>
  );
}
