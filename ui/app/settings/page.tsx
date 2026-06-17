"use client";

import { Card, PageHeader, Badge, Button } from "@/components/ui";
import { usePoll, type Overview, type NodeInfo } from "@/lib/api";

export default function SettingsPage() {
  const { data: ov } = usePoll<Overview>("/v1/overview", 5000);
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 5000);
  return (
    <div>
      <PageHeader title="Settings" desc="Team & cloud configuration" />
      <Card className="mb-4 p-5">
        <div className="mb-3 text-sm font-medium">Cloud</div>
        <Row label="Node" value={ov?.node ?? "—"} />
        <Row label="Region" value={ov?.region ?? "—"} />
        <Row label="Plan" value={<Badge tone="blue">{ov?.concurrency.plan ?? "—"}</Badge>} />
        <Row label="Nodes in mesh" value={String(ov?.nodes ?? "—")} />
        <Row label="Regions" value={(ov?.regions ?? []).join(", ") || "—"} />
      </Card>
      <Card className="mb-4 p-5">
        <div className="mb-3 text-sm font-medium">Nodes</div>
        <div className="flex flex-col gap-2">
          {(nodes ?? []).map((n) => (
            <div key={n.id} className="flex items-center justify-between rounded-lg border border-border px-3 py-2 text-sm">
              <span className="font-medium">{n.name} <span className="text-muted">· {n.region}</span></span>
              <Badge tone={n.is_self ? "green" : "default"}>{n.is_self ? "this node" : "peer"}</Badge>
            </div>
          ))}
        </div>
      </Card>
      <Card className="p-5">
        <div className="mb-1 text-sm font-medium text-red-600">Danger Zone</div>
        <p className="mb-3 text-sm text-secondary">Purge the CDN cache for all deployments.</p>
        <Button variant="danger" onClick={() => fetch("/cloud/v1/cdn/purge", { method: "POST" })}>Purge CDN cache</Button>
      </Card>
    </div>
  );
}

function Row({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between border-b border-border py-2.5 text-sm last:border-0">
      <span className="text-secondary">{label}</span>
      <span className="font-medium">{value}</span>
    </div>
  );
}
