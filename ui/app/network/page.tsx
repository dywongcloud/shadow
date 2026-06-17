"use client";

import { useState } from "react";
import { Globe2, Server, Share2, ShieldCheck, Database, Network } from "lucide-react";
import { Card, Badge, Button, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type NodeInfo, type Overview, type AnycastTable, type RateLimitStats } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

interface ClusterStatus { term: number; leader: string; is_leader: boolean; members: string[]; consensus: string }

export default function NetworkPage() {
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 3000);
  const { data: ov } = usePoll<Overview>("/v1/overview", 4000);
  const { data: cluster } = usePoll<ClusterStatus>("/v1/cluster", 3000);
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
        <Card>
          <div className="mb-2 flex items-center gap-2 text-sm font-medium"><ShieldCheck className="h-4 w-4" /> Coordinated cluster</div>
          <p className="text-sm text-secondary">Nodes agree on a leader so deployments stay consistent across regions.</p>
          {cluster && (
            <div className="mt-3 flex flex-col gap-1.5 text-xs">
              <div className="flex justify-between"><span className="text-muted">Leader</span><span className="font-mono">{cluster.leader}{cluster.is_leader ? " (this)" : ""}</span></div>
              <div className="flex justify-between"><span className="text-muted">Term</span><span className="font-mono">{cluster.term}</span></div>
              <div className="flex justify-between"><span className="text-muted">Members</span><span className="font-mono">{cluster.members.length}</span></div>
            </div>
          )}
        </Card>
      </div>

      <AnycastRouting />

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

/** Anycast routing table + L7 DDoS rate limiting. */
function AnycastRouting() {
  const { data: any } = usePoll<AnycastTable>("/v1/anycast", 3000);
  const { data: rl, refresh } = usePoll<RateLimitStats>("/v1/ratelimit", 4000);
  const [limit, setLimit] = useState("");
  const [windowS, setWindowS] = useState("");

  async function save(enabled: boolean) {
    await apiSend("PUT", "/v1/ratelimit", {
      enabled,
      limit: Number(limit) || rl?.limit || 100,
      window_ms: (Number(windowS) || (rl ? rl.window_ms / 1000 : 10)) * 1000,
    });
    refresh();
  }

  return (
    <div className="mb-6 grid grid-cols-1 gap-4 lg:grid-cols-[1fr_360px]">
      {/* Anycast table */}
      <Card>
        <div className="mb-1 flex items-center gap-2 text-sm font-medium"><Globe2 className="h-4 w-4" /> Anycast Routing</div>
        <p className="mb-4 text-sm text-secondary">
          Requests are routed to the lowest-latency healthy node (region-preferred), with automatic
          failover — the network-hop equivalent of anycast, over the iroh mesh.
        </p>
        <Table>
          <thead><tr><Th>Node</Th><Th>Region</Th><Th>Latency</Th><Th>Health</Th><Th>Routing</Th></tr></thead>
          <tbody>
            {(any?.table ?? []).map((n) => (
              <tr key={n.id}>
                <Td className="font-medium">{n.name}</Td>
                <Td><Badge tone="blue">{n.region}</Badge></Td>
                <Td className="font-mono text-xs">{n.is_self ? "0ms (local)" : `${n.latency_ms ?? 0}ms`}</Td>
                <Td>{n.healthy ? <Badge tone="green">healthy</Badge> : <Badge tone="red">down</Badge>}</Td>
                <Td>{any?.selected === n.name ? <Badge tone="green">● serving</Badge> : <span className="text-xs text-muted">standby</span>}</Td>
              </tr>
            ))}
          </tbody>
        </Table>
      </Card>

      {/* L7 DDoS rate limiting */}
      <Card>
        <div className="mb-1 flex items-center gap-2 text-sm font-medium"><ShieldCheck className="h-4 w-4" /> L7 DDoS Mitigation</div>
        <p className="mb-4 text-sm text-secondary">Per-IP rate limiting at the edge — floods are shed (429) before any compute.</p>
        <div className="mb-3 flex items-center justify-between text-sm">
          <span className="text-secondary">Status</span>
          <Badge tone={rl?.enabled ? "green" : "default"}>{rl?.enabled ? "enabled" : "off"}</Badge>
        </div>
        <div className="mb-3 grid grid-cols-2 gap-2">
          <div>
            <label className="mb-1 block text-[11px] uppercase tracking-wide text-muted">Limit / IP</label>
            <input value={limit} onChange={(e) => setLimit(e.target.value.replace(/[^0-9]/g, ""))} placeholder={String(rl?.limit ?? 100)} className="w-full rounded-md border border-border bg-card px-2 py-1.5 text-sm focus:outline-none" />
          </div>
          <div>
            <label className="mb-1 block text-[11px] uppercase tracking-wide text-muted">Window (s)</label>
            <input value={windowS} onChange={(e) => setWindowS(e.target.value.replace(/[^0-9]/g, ""))} placeholder={String((rl?.window_ms ?? 10000) / 1000)} className="w-full rounded-md border border-border bg-card px-2 py-1.5 text-sm focus:outline-none" />
          </div>
        </div>
        <div className="mb-4 flex items-center justify-between text-sm">
          <span className="text-secondary">Blocked (total)</span>
          <span className="font-semibold tabular-nums text-red-500">{rl?.blocked_total ?? 0}</span>
        </div>
        <div className="flex gap-2">
          <Button onClick={() => save(true)} className="flex-1">Apply</Button>
          <Button variant="outline" onClick={() => save(!rl?.enabled)}>{rl?.enabled ? "Disable" : "Enable"}</Button>
        </div>
      </Card>
    </div>
  );
}
