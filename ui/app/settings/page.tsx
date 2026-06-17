"use client";

import Link from "next/link";
import { Webhook, Bell, ChevronRight } from "lucide-react";
import { Card, PageHeader, Badge, Button } from "@/components/ui";
import { ActivityFeed } from "@/components/activity-feed";
import { usePoll, type Overview, type NodeInfo } from "@/lib/api";

export default function SettingsPage() {
  const { data: ov } = usePoll<Overview>("/v1/overview", 5000);
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 5000);

  return (
    <div>
      <PageHeader title="Team Settings" desc="Team configuration, cloud, and activity" />

      <div className="mb-6 grid grid-cols-1 gap-4 sm:grid-cols-2">
        <NavCard href="/settings/webhooks" icon={<Webhook className="h-4 w-4" />} title="Webhooks" desc="Send signed events to your endpoints." />
        <NavCard href="/settings/notifications" icon={<Bell className="h-4 w-4" />} title="Notifications" desc="Web, email, push & SMS preferences." />
      </div>

      <General ov={ov} nodes={nodes} />
      <div className="mt-10">
        <h2 className="mb-4 text-xl font-semibold tracking-tight">Activity</h2>
        <ActivityFeed />
      </div>
    </div>
  );
}

function NavCard({ href, icon, title, desc }: { href: string; icon: React.ReactNode; title: string; desc: string }) {
  return (
    <Link href={href}>
      <Card className="flex items-center justify-between p-4 transition-shadow hover:shadow-pop">
        <div className="flex items-center gap-3">
          <span className="flex h-9 w-9 items-center justify-center rounded-lg bg-subtle text-secondary">{icon}</span>
          <div>
            <div className="text-sm font-medium">{title}</div>
            <div className="text-xs text-secondary">{desc}</div>
          </div>
        </div>
        <ChevronRight className="h-4 w-4 text-muted" />
      </Card>
    </Link>
  );
}

function General({ ov, nodes }: { ov: Overview | null; nodes: NodeInfo[] | null }) {
  return (
    <div>
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
