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

      {/* P2P mesh map */}
      <Card className="mb-6 overflow-hidden">
        <div className="mb-2 flex items-center justify-between">
          <div className="flex items-center gap-2 text-sm font-medium"><Network className="h-4 w-4" /> P2P Mesh</div>
          <div className="flex items-center gap-3 text-xs text-muted">
            <span className="flex items-center gap-1.5"><span className="inline-block h-2.5 w-2.5 rotate-45 rounded-[2px] bg-[#34c759]" /> node</span>
            <span className="flex items-center gap-1.5"><span className="inline-block h-2.5 w-2.5 rotate-45 rounded-[2px] bg-[#9ca3af]" /> unhealthy</span>
          </div>
        </div>
        <MeshDiagram nodes={nodes ?? []} />
        <p className="mt-2 text-center text-xs text-muted">
          Every node runs a gateway + Fluid pool, fully meshed over iroh QUIC. The function tunnel protocol
          rides those streams, so a gateway on one node can serve an instance on any other, anywhere reachable.
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

      <div className="mb-2 flex items-center justify-between">
        <span className="text-sm font-medium text-fg">Nodes</span>
        <span className="text-xs text-muted">
          Cluster capacity:{" "}
          <span className="font-medium text-secondary">
            {(nodes ?? []).reduce((a, n) => a + (n.cpu_cores ?? 0), 0)} vCPU ·{" "}
            {fmtMem((nodes ?? []).reduce((a, n) => a + (n.mem_total_mb ?? 0), 0))} ·{" "}
            {(nodes ?? []).reduce((a, n) => a + (n.disk_total_gb ?? 0), 0)} GB
          </span>
        </span>
      </div>
      <Table>
        <thead><tr><Th>Node</Th><Th>Region</Th><Th>Endpoint</Th><Th>vCPU</Th><Th>Memory</Th><Th>Disk</Th><Th>Role</Th><Th>Seen</Th></tr></thead>
        <tbody>
          {(nodes ?? []).map((n) => (
            <tr key={n.id}>
              <Td className="font-medium">{n.name}</Td>
              <Td><Badge tone="blue">{n.region}</Badge></Td>
              <Td className="font-mono text-xs text-secondary">{n.public_url}</Td>
              <Td className="tabular-nums">{n.cpu_cores ?? "—"}</Td>
              <Td className="tabular-nums">{fmtMem(n.mem_total_mb)}</Td>
              <Td className="tabular-nums">{n.disk_total_gb ? `${n.disk_total_gb} GB` : "—"}</Td>
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

/** Format a memory size given in MB. */
function fmtMem(mb?: number): string {
  if (!mb) return "—";
  if (mb >= 1024) {
    const gb = mb / 1024;
    return `${gb >= 100 ? Math.round(gb) : gb.toFixed(gb % 1 ? 1 : 0)} GB`;
  }
  return `${mb} MB`;
}

/**
 * P2P mesh map: nodes evenly on a circle, fully connected by curved orbital wire
 * lines (blue in light mode, white in dark — see the `stroke-*` classes), each
 * node a 3D green isometric cube with its name below. Pure SVG, theme-aware, no deps.
 */
function MeshDiagram({ nodes }: { nodes: NodeInfo[] }) {
  const W = 760;
  const H = 560;
  const cx = W / 2;
  const cy = H / 2;
  const R = Math.min(W, H) * 0.36;
  const n = nodes.length;

  if (n === 0) {
    return <div className="py-20 text-center text-sm text-secondary">Discovering nodes…</div>;
  }

  // Node positions on the circle (start at top, go clockwise). A single node sits
  // in the center.
  const pos = nodes.map((_, i) => {
    if (n === 1) return { x: cx, y: cy };
    const a = -Math.PI / 2 + (i * 2 * Math.PI) / n;
    return { x: cx + R * Math.cos(a), y: cy + R * Math.sin(a) };
  });

  // Full-mesh edges as quadratic Béziers bowed outward from the center, so the
  // overlapping arcs form the "atomic orbit" look.
  const edges: string[] = [];
  for (let i = 0; i < n; i++) {
    for (let j = i + 1; j < n; j++) {
      const p = pos[i];
      const q = pos[j];
      const mx = (p.x + q.x) / 2;
      const my = (p.y + q.y) / 2;
      let nx = mx - cx;
      let ny = my - cy;
      const ol = Math.hypot(nx, ny);
      if (ol < R * 0.2) {
        // Near-diameter chord (midpoint ~ center): bow perpendicular instead so it
        // still curves rather than collapsing to a straight diameter.
        const dx = q.x - p.x;
        const dy = q.y - p.y;
        const dl = Math.hypot(dx, dy) || 1;
        nx = -dy / dl;
        ny = dx / dl;
      } else {
        nx /= ol;
        ny /= ol;
      }
      const bow = R * 0.42;
      const ctrlX = mx + nx * bow;
      const ctrlY = my + ny * bow;
      edges.push(
        `M ${p.x.toFixed(1)} ${p.y.toFixed(1)} Q ${ctrlX.toFixed(1)} ${ctrlY.toFixed(1)} ${q.x.toFixed(1)} ${q.y.toFixed(1)}`,
      );
    }
  }

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="mx-auto block w-full" style={{ maxHeight: 540 }} preserveAspectRatio="xMidYMid meet">
      {/* mesh wires — blue (light) / white (dark) */}
      <g fill="none" className="stroke-[#2f7fea] dark:stroke-white" strokeWidth={1.1} strokeOpacity={0.55}>
        {edges.map((d, i) => (
          <path key={i} d={d} />
        ))}
      </g>
      {/* nodes */}
      {nodes.map((node, i) => (
        <g key={node.id} transform={`translate(${pos[i].x.toFixed(1)} ${pos[i].y.toFixed(1)})`}>
          <CubeIcon self={!!node.is_self} healthy={node.healthy !== false} />
          <text y={38} textAnchor="middle" style={{ fontSize: 13 }} className="fill-neutral-700 dark:fill-neutral-200">
            {node.name}
          </text>
        </g>
      ))}
    </svg>
  );
}

/** A 3D green isometric cube node icon (gray when unhealthy), centered at (0,0). */
function CubeIcon({ self, healthy }: { self: boolean; healthy: boolean }) {
  const r = 18;
  const top = healthy ? "#3ad15f" : "#b0b6bd";
  const left = healthy ? "#23a64e" : "#8a9098";
  const right = healthy ? "#178a3d" : "#6c727a";
  return (
    <g>
      {self && (
        <circle r={r + 9} fill="none" className="stroke-[#34c759]" strokeOpacity={0.55} strokeWidth={1.5} strokeDasharray="3 3" />
      )}
      {/* top, left, right faces */}
      <path d={`M 0 ${-r} L ${r} ${-r / 2} L 0 0 L ${-r} ${-r / 2} Z`} fill={top} stroke={right} strokeWidth={0.8} />
      <path d={`M ${-r} ${-r / 2} L 0 0 L 0 ${r} L ${-r} ${r / 2} Z`} fill={left} stroke={right} strokeWidth={0.8} />
      <path d={`M ${r} ${-r / 2} L 0 0 L 0 ${r} L ${r} ${r / 2} Z`} fill={right} stroke={right} strokeWidth={0.8} />
      {/* chip detail on the top face */}
      <path d={`M 0 ${-r * 0.5} L ${r * 0.48} ${-r * 0.25} L 0 0 L ${-r * 0.48} ${-r * 0.25} Z`} fill="none" stroke={right} strokeOpacity={0.55} strokeWidth={1} />
    </g>
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
                <Td>{(any?.serving?.[n.name] ?? 0) > 0 ? <Badge tone="green">● serving</Badge> : <span className="text-xs text-muted">standby</span>}</Td>
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
