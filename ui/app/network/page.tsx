"use client";

import { Globe2, Server, Share2, ShieldCheck, Database, Network } from "lucide-react";
import { Card, Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { usePoll, type NodeInfo, type Overview } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function NetworkPage() {
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 3000);
  const { data: ov } = usePoll<Overview>("/v1/overview", 4000);
  const regions = Array.from(new Set((nodes ?? []).map((n) => n.region))).sort();

  return (
    <div>
      <PageHeader
        title="Network"
        desc="Your nodes form a peer-to-peer mesh over iroh QUIC. Gateways reach instances by endpoint id — no public IPs, NAT traversal and relay fallback handled automatically."
      />

      <div className="mb-6 grid grid-cols-2 gap-4 md:grid-cols-4">
        <Stat icon={<Server className="h-4 w-4" />} label="Nodes" value={nodes?.length ?? "—"} />
        <Stat icon={<Globe2 className="h-4 w-4" />} label="Regions" value={regions.length || "—"} />
        <Stat icon={<Share2 className="h-4 w-4" />} label="Transport" value="iroh QUIC" />
        <Stat icon={<Database className="h-4 w-4" />} label="State store" value="replicated" />
      </div>

      {/* Mesh visualization */}
      <Card className="mb-6">
        <div className="mb-4 flex items-center gap-2 text-sm font-medium">
          <Network className="h-4 w-4" /> P2P Mesh
        </div>
        <div className="flex flex-wrap items-center justify-center gap-6 py-6">
          {(nodes ?? []).map((n, i) => (
            <div key={n.id} className="flex flex-col items-center gap-2">
              <div
                className={`flex h-16 w-16 items-center justify-center rounded-full border-2 ${
                  n.is_self ? "border-green bg-green/10 text-green" : "border-border-strong bg-subtle text-secondary"
                }`}
              >
                <Server className="h-6 w-6" />
              </div>
              <div className="text-center">
                <div className="text-sm font-medium">{n.name}</div>
                <Badge tone="blue">{n.region}</Badge>
              </div>
              <div className="font-mono text-[10px] text-muted">{(n.peer_id || n.id).slice(0, 10)}…</div>
            </div>
          ))}
          {!nodes?.length && <div className="text-sm text-secondary">Discovering nodes…</div>}
        </div>
        <p className="mt-2 text-center text-xs text-muted">
          Every node runs a gateway + Fluid pool. The function tunnel protocol rides iroh streams, so a
          gateway on one node can serve an instance on another, anywhere reachable.
        </p>
      </Card>

      {/* Architecture rows */}
      <div className="mb-6 grid grid-cols-1 gap-4 md:grid-cols-3">
        <Arch icon={<Share2 className="h-4 w-4" />} title="P2P transport (iroh)" desc="Multiplexed function tunnels over QUIC; dialed by public-key endpoint id with relay fallback." />
        <Arch icon={<Database className="h-4 w-4" />} title="Replicated state" desc="Platform records persist to disk and replicate across the mesh (guardian-db, iroh-native)." />
        <Arch icon={<ShieldCheck className="h-4 w-4" />} title="Coordinated cluster" desc="Nodes agree on cluster membership and routing so deployments stay consistent across regions." />
      </div>

      <div className="mb-2 text-sm font-medium text-fg">Nodes</div>
      <Table>
        <thead><tr><Th>Node</Th><Th>Region</Th><Th>Endpoint</Th><Th>Role</Th><Th>Seen</Th></tr></thead>
        <tbody>
          {(nodes ?? []).map((n) => (
            <tr key={n.id}>
              <Td className="font-medium">{n.name}</Td>
              <Td><Badge tone="blue">{n.region}</Badge></Td>
              <Td className="font-mono text-xs text-secondary">{n.public_url}</Td>
              <Td>{n.is_self ? <Badge tone="green">this node</Badge> : <Badge>peer</Badge>}</Td>
              <Td className="text-secondary">{timeAgo(n.last_seen_ms)} ago</Td>
            </tr>
          ))}
        </tbody>
      </Table>
      <Card className="mt-4 text-sm text-secondary">
        Join another MacBook to the mesh:{" "}
        <code className="font-mono text-xs">hive-cloud --region fra1 --name node-b --peer http://&lt;this-ip&gt;:8786</code>
      </Card>
    </div>
  );
}

function Stat({ icon, label, value }: { icon: React.ReactNode; label: string; value: React.ReactNode }) {
  return (
    <Card className="flex flex-col gap-1">
      <span className="flex items-center gap-1.5 text-xs font-medium uppercase tracking-wide text-muted">{icon}{label}</span>
      <span className="text-2xl font-semibold text-fg">{value}</span>
    </Card>
  );
}

function Arch({ icon, title, desc }: { icon: React.ReactNode; title: string; desc: string }) {
  return (
    <Card>
      <div className="mb-2 flex items-center gap-2 text-sm font-medium">{icon}{title}</div>
      <p className="text-sm text-secondary">{desc}</p>
    </Card>
  );
}
