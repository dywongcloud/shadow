"use client";

import { useState } from "react";
import { Globe2, Server, Share2, ShieldCheck, Database, Network, Boxes } from "lucide-react";
import { Card, Badge, Button, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type NodeInfo, type AnycastTable, type RateLimitStats } from "@/lib/api";
import { SATELLITE_ONLINE_COLOR, SATELLITE_DEGRADED_COLOR } from "@/components/region-map";
import type { BrowserPresence } from "@/lib/run-node-client";
import { timeAgo } from "@/lib/utils";

interface ClusterStatus { term: number; leader: string; is_leader: boolean; members: string[]; consensus: string }

const SHARD_HOLDER_COLOR = "#f59e0b";

/** A presence record as the constellation reads it; `node_name` and `shard_eligible` are server-derived and optional here
 *  because the dashboard and the backend deploy independently (a fresh UI talks to nodes that predate them). */
type BrowserNode = BrowserPresence & { node_name?: string; shard_eligible?: boolean };

/** The backend's own `bn-<adj>-<noun>-<tag>` name for this peer, else the endpoint-id prefix; never the tenant-scoped
 *  `display_label`, which names the OWNER rather than the node. */
function browserNodeName(p: BrowserNode): string {
  return p.node_name || `bn-${p.endpoint_id.slice(0, 8)}`;
}

export default function NetworkPage() {
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 10000);
  const { data: cluster } = usePoll<ClusterStatus>("/v1/cluster", 10000);
  const { data: presenceFeed } = usePoll<{ presence: BrowserNode[] }>("/v1/browser/presence", 8000);
  const presence = presenceFeed?.presence ?? [];
  const regions = Array.from(new Set((nodes ?? []).map((n) => n.region))).sort();
  const fleetCount = nodes ? nodes.length : null;
  const browserCount = presence.length;
  const nodeTotal = fleetCount === null ? null : fleetCount + browserCount;

  return (
    <div>
      <PageHeader
        title="Constellation"
        desc="Your nodes form a peer-to-peer mesh over iroh QUIC. Gateways reach instances by endpoint id — no public IPs, NAT traversal and relay fallback handled automatically."
      />

      <div className="mb-6 grid grid-cols-2 gap-4 md:grid-cols-4">
        <Stat
          icon={<Server className="h-4 w-4" />}
          label="Nodes"
          value={nodeTotal ?? "—"}
          hint={
            fleetCount === null
              ? undefined
              : browserCount > 0
                ? `${fleetCount} fleet + ${browserCount} browser`
                : `${fleetCount} fleet`
          }
        />
        <Stat icon={<Globe2 className="h-4 w-4" />} label="Regions" value={regions.length || "—"} />
        <Stat icon={<Share2 className="h-4 w-4" />} label="Transport" value="iroh QUIC" />
        <Stat icon={<Database className="h-4 w-4" />} label="State store" value="replicated" />
      </div>

      <Card className="mb-6 overflow-hidden">
        <div className="mb-2 flex items-center justify-between">
          <div className="flex items-center gap-2 text-sm font-medium"><Network className="h-4 w-4" /> P2P Mesh</div>
          <div className="flex items-center gap-3 text-xs text-muted">
            <span className="flex items-center gap-1.5"><span className="inline-block h-2.5 w-2.5 rotate-45 rounded-[2px] bg-[#34c759]" /> node</span>
            <span className="flex items-center gap-1.5"><span className="inline-block h-2.5 w-2.5 rotate-45 rounded-[2px] bg-[#3b82f6]" /> GPU</span>
            <span className="flex items-center gap-1.5"><span className="inline-block h-2.5 w-2.5 rotate-45 rounded-[2px] bg-[#9ca3af]" /> unhealthy</span>
            <span className="flex items-center gap-1.5" title="Low-trust volunteer browser peers — the same cube as a fleet node, same size and place on the ring, teal instead of green; counted in the node total but they do not host deployments. Named bn-<adjective>-<noun>-<tag>, derived server-side from the peer's proven endpoint id.">
              <span className="inline-block h-2.5 w-2.5 rotate-45 rounded-[2px]" style={{ background: SATELLITE_ONLINE_COLOR }} /> browser node <span className="font-mono text-[10px] opacity-70">bn-*</span>
            </span>
            <span className="flex items-center gap-1.5" title="This browser node is eligible to hold small fragments of global platform state (see Storage shards below). Eligibility is server-derived from a platform-admin session — a browser cannot claim it.">
              <span className="inline-block h-2.5 w-2.5 rounded-full border border-dashed" style={{ borderColor: SHARD_HOLDER_COLOR }} /> shard holder
            </span>
          </div>
        </div>
        <MeshDiagram nodes={nodes ?? []} presence={presence} />
        <p className="mt-2 text-center text-xs text-muted">
          Every node runs a gateway + Fluid pool, fully meshed over iroh QUIC. The function tunnel protocol
          rides those streams, so a gateway on one node can serve an instance on any other, anywhere reachable.
          {presence.length > 0 && (
            <> Browser nodes are admitted peers on the same ring — low-trust edge capacity attached over the relay, counted in the node total but never hosting deployments.
            Each carries its own <span className="font-mono">bn-…</span> name, derived server-side from its proven endpoint id and stable across reconnects. A thin teal line marks the fleet node a peer relays through; the peer itself sits in its own slot.</>
          )}
        </p>
      </Card>

      <StorageShards />

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
        <thead><tr><Th>Node</Th><Th>Region</Th><Th>Endpoint</Th><Th>vCPU</Th><Th>Memory</Th><Th>Disk</Th><Th>GPU</Th><Th>Role</Th><Th>Seen</Th></tr></thead>
        <tbody>
          {(nodes ?? []).map((n) => (
            <tr key={n.id}>
              <Td className="font-medium">{n.name}</Td>
              <Td><Badge tone="blue">{n.region}</Badge></Td>
              <Td className="font-mono text-xs text-secondary">{n.public_url ?? "—"}</Td>
              <Td className="tabular-nums">{n.cpu_cores ?? "—"}</Td>
              <Td className="tabular-nums">{fmtMem(n.mem_total_mb)}</Td>
              <Td className="tabular-nums">{n.disk_total_gb ? `${n.disk_total_gb} GB` : "—"}</Td>
              <Td>{(n.gpu_count ?? 0) > 0
                ? <Badge tone="purple">{n.gpu_count}× {n.gpu_model ?? "GPU"}</Badge>
                : <span className="text-secondary">—</span>}</Td>
              <Td>{n.is_self ? <Badge tone="green">this node</Badge> : <Badge>peer</Badge>}</Td>
              <Td className="text-secondary">{timeAgo(n.last_seen_ms)}</Td>
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

/** An unhealthy or unmeasured peer reports a sentinel latency (hive-edge `region.rs`: `u64::MAX`, sorting last): anything
 *  at or past a day is that sentinel, never a round trip. */
const LATENCY_SENTINEL_FLOOR_MS = 24 * 60 * 60 * 1000;

function formatLatency(ms?: number): string {
  if (ms === undefined || ms === null || !Number.isFinite(ms)) return "—";
  if (ms >= LATENCY_SENTINEL_FLOOR_MS) return "unreachable";
  return `${ms}ms`;
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

function MeshDiagram({ nodes, presence }: { nodes: NodeInfo[]; presence: BrowserNode[] }) {
  const W = 760;
  const H = 560;
  const cx = W / 2;
  const cy = H / 2;
  const R = Math.min(W, H) * 0.36;
  const n = nodes.length;

  if (n === 0 && presence.length === 0) {
    return <div className="py-20 text-center text-sm text-secondary">Discovering nodes…</div>;
  }

  const MAX_BROWSER_NODES = 60;
  const MAX_LABELLED_BROWSER_NODES = 24;
  const sortedPresence = [...presence].sort((a, b) => (a.endpoint_id < b.endpoint_id ? -1 : a.endpoint_id > b.endpoint_id ? 1 : 0));
  const browserNodes = sortedPresence.slice(0, MAX_BROWSER_NODES);
  const browserOverflow = sortedPresence.length - browserNodes.length;
  const showBrowserNames = browserNodes.length > 0 && browserNodes.length <= MAX_LABELLED_BROWSER_NODES;

  const total = n + browserNodes.length;
  const ringXY = (i: number) => {
    if (total === 1) return { x: cx, y: cy };
    const a = -Math.PI / 2 + (i * 2 * Math.PI) / total;
    return { x: cx + R * Math.cos(a), y: cy + R * Math.sin(a) };
  };
  const pos = nodes.map((_, i) => ringXY(i));
  const browserPos = browserNodes.map((_, k) => ringXY(n + k));

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

  const firstRelayHost = (relays?: string | null): string => {
    if (!relays) return "";
    const first = relays.split(",")[0]?.trim() ?? "";
    if (!first) return "";
    try { return new URL(first).hostname; } catch {}
    return /^[a-z0-9.-]+$/i.test(first) ? first : "";
  };
  /** Subject prefix of the fleet's own headless browser nodes (minted server-side, so a browser cannot claim it); the
   *  suffix names the fleet host the node runs on. */
  const FLEET_BROWSER_SUBJECT = "fleet-browser-node:";
  const hostNodeOf = (p: BrowserNode): string => {
    const subject = (p as { subject?: string }).subject ?? "";
    return subject.startsWith(FLEET_BROWSER_SUBJECT)
      ? subject.slice(FLEET_BROWSER_SUBJECT.length).trim()
      : "";
  };
  const anchorOf = browserNodes.map((p) => {
    const hostNode = hostNodeOf(p);
    if (hostNode) {
      const byHost = nodes.findIndex((nd) => nd.name === hostNode);
      if (byHost >= 0) return byHost;
    }
    const hintHost = firstRelayHost(p.relay_hint);
    if (!hintHost) return -1;
    return nodes.findIndex((nd) => firstRelayHost(nd.relay_url) === hintHost || (Boolean(nd.name) && hintHost.includes(nd.name)));
  });

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="mx-auto block w-full" style={{ maxHeight: 540 }} preserveAspectRatio="xMidYMid meet">
      <style>{`
        @keyframes hiveMeshFlow { to { stroke-dashoffset: -24; } }
        .hive-mesh-wire {
          stroke-dasharray: 6 6;
          animation: hiveMeshFlow 1.8s linear infinite;
        }
        @media (prefers-reduced-motion: reduce) {
          .hive-mesh-wire { animation: none; }
        }
      `}</style>
      <g fill="none" className="stroke-[#2f7fea] dark:stroke-white" strokeWidth={1.1} strokeOpacity={0.55} strokeLinecap="round">
        {edges.map((d, i) => (
          <path key={i} d={d} className="hive-mesh-wire" style={{ animationDelay: `${-((i * 0.13) % 1.8).toFixed(2)}s` }} />
        ))}
      </g>
      <g fill="none" strokeLinecap="round">
        {browserNodes.map((p, k) => {
          const ai = anchorOf[k];
          if (ai < 0) return null;
          return (
            <line
              key={p.endpoint_id}
              x1={pos[ai].x}
              y1={pos[ai].y}
              x2={browserPos[k].x}
              y2={browserPos[k].y}
              stroke={p.state === "degraded" || p.state === "suspended" ? SATELLITE_DEGRADED_COLOR : SATELLITE_ONLINE_COLOR}
              strokeWidth={1}
              strokeOpacity={0.5}
            />
          );
        })}
      </g>
      {nodes.map((node, i) => (
        <g key={node.id} transform={`translate(${pos[i].x.toFixed(1)} ${pos[i].y.toFixed(1)})`}>
          <CubeIcon self={!!node.is_self} healthy={node.healthy !== false} gpu={(node.gpu_count ?? 0) > 0} />
          <text y={38} textAnchor="middle" style={{ fontSize: 13 }} className="fill-neutral-700 dark:fill-neutral-200">
            {node.name}
          </text>
        </g>
      ))}
      {browserNodes.map((p, k) => {
        const degraded = p.state === "degraded" || p.state === "suspended";
        const faces = degraded
          ? { top: "#cbd5e1", left: SATELLITE_DEGRADED_COLOR, right: "#64748b" }
          : { top: "#7dd3fc", left: SATELLITE_ONLINE_COLOR, right: "#0ea5e9" };
        const name = browserNodeName(p);
        return (
          <g key={p.endpoint_id} transform={`translate(${browserPos[k].x.toFixed(1)} ${browserPos[k].y.toFixed(1)})`}>
            <CubeIcon
              self={false}
              healthy={!degraded}
              faces={faces}
              ring={p.shard_eligible ? SHARD_HOLDER_COLOR : undefined}
            />
            {showBrowserNames && (
              <text y={38} textAnchor="middle" style={{ fontSize: 13 }} className="fill-neutral-700 dark:fill-neutral-200">
                {name}
              </text>
            )}
            <title>{`${name} · browser node (low-trust)${p.state ? ` · ${p.state}` : ""}${p.shard_eligible ? " · state-shard holder" : ""}\nendpoint ${p.endpoint_id}`}</title>
          </g>
        );
      })}
      {browserOverflow > 0 && (
        <text x={W - 8} y={H - 10} textAnchor="end" style={{ fontSize: 11 }} className="fill-neutral-500 dark:fill-neutral-400">
          {`+${browserOverflow} more browser node${browserOverflow === 1 ? "" : "s"} not drawn`}
        </text>
      )}
    </svg>
  );
}

/** Isometric cube node icon: blue for a GPU node (`gpu_count > 0`, the fleet-wide GPU hue), green otherwise, gray when
 *  unhealthy; health wins over capability. */
function CubeIcon({
  self,
  healthy,
  gpu,
  size = 18,
  faces,
  ring,
}: {
  self: boolean;
  healthy: boolean;
  gpu?: boolean;
  /** Cube radius. */
  size?: number;
  /** Explicit top/left/right face colors overriding the health/GPU palette (browser peers render teal). */
  faces?: { top: string; left: string; right: string };
  /** Dashed orbit ring in this color regardless of `self`: marks a browser peer eligible to hold global-state fragments. */
  ring?: string;
}) {
  const r = size;
  const top = faces ? faces.top : !healthy ? "#b0b6bd" : gpu ? "#60a5fa" : "#3ad15f";
  const left = faces ? faces.left : !healthy ? "#8a9098" : gpu ? "#3b82f6" : "#23a64e";
  const right = faces ? faces.right : !healthy ? "#6c727a" : gpu ? "#1d4ed8" : "#178a3d";
  return (
    <g>
      {(self || ring) && (
        <circle
          r={r + 9}
          fill="none"
          stroke={ring ?? (gpu ? "#3b82f6" : "#34c759")}
          strokeOpacity={0.55}
          strokeWidth={1.5}
          strokeDasharray="3 3"
        />
      )}
      <path d={`M 0 ${-r} L ${r} ${-r / 2} L 0 0 L ${-r} ${-r / 2} Z`} fill={top} stroke={right} strokeWidth={0.8} />
      <path d={`M ${-r} ${-r / 2} L 0 0 L 0 ${r} L ${-r} ${r / 2} Z`} fill={left} stroke={right} strokeWidth={0.8} />
      <path d={`M ${r} ${-r / 2} L 0 0 L 0 ${r} L ${r} ${r / 2} Z`} fill={right} stroke={right} strokeWidth={0.8} />
      <path d={`M 0 ${-r * 0.5} L ${r * 0.48} ${-r * 0.25} L 0 0 L ${-r * 0.48} ${-r * 0.25} Z`} fill="none" stroke={right} strokeOpacity={0.55} strokeWidth={1} />
    </g>
  );
}

interface ShardFragment { key: string; store: string; index: number; bytes: number; digest: string }
interface ShardHolding { endpoint_id: string; node_name: string; max_bytes: number; held_bytes: number; fragments: ShardFragment[] }
interface ShardRefusal { reason: string; fragment: string; endpoint_id: string; detail: string }
interface ShardPlan {
  params: { enabled: boolean; max_bytes: number; fragment_bytes: number; replication_factor: number; stores: string[] };
  membership: string[];
  membership_digest: string;
  plan_digest: string;
  fragments_total: number;
  fragment_bytes_total: number;
  replicas_wanted: number;
  replicas_placed: number;
  under_replicated_fragments: number;
  unplaced_fragments: number;
  holdings: ShardHolding[];
  refusals: ShardRefusal[];
  refusals_total: number;
}
interface ShardTrust {
  model: string;
  proves: string[];
  does_not_prove: string[];
  options_for_a_real_possession_guarantee: string[];
  implemented_here: string;
}

function fmtBytes(n: number): string {
  if (!n) return "0 B";
  if (n >= 1024 * 1024 * 1024) return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  if (n >= 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  if (n >= 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${n} B`;
}

/** Which global-state fragments each browser node holds (`GET /v1/browser/shards`, operator-only: the poll is gated on the
 *  `hive_is_owner` courtesy flag, not enforcement); renders nothing without a plan. */
function StorageShards() {
  const isOwner = typeof window !== "undefined" && localStorage.getItem("hive_is_owner") === "1";
  const { data } = usePoll<{ plan: ShardPlan; trust: ShardTrust }>("/v1/browser/shards", 20000, isOwner);
  const [showTrust, setShowTrust] = useState(false);
  if (!data?.plan) return null;
  const { plan, trust } = data;
  const held = plan.holdings.reduce((a, h) => a + h.held_bytes, 0);
  const shortfall = plan.unplaced_fragments + plan.under_replicated_fragments;

  return (
    <Card className="mb-6">
      <div className="mb-1 flex items-center justify-between">
        <div className="flex items-center gap-2 text-sm font-medium"><Boxes className="h-4 w-4" /> Storage shards</div>
        <Badge tone={plan.params.enabled ? (shortfall > 0 ? "amber" : "green") : "default"}>
          {!plan.params.enabled ? "disabled" : shortfall > 0 ? "under-replicated" : "placed"}
        </Badge>
      </div>
      <p className="mb-4 text-sm text-secondary">
        A browser cannot hold the platform&apos;s replicated state, so it holds small fragments of it. Every
        node derives the same assignment with no coordinator: rendezvous (HRW) hashing over the eligible
        browser peers, keyed on the fragment — the same hash container placement and the inference
        coordinator already agree through. A fragment that would push a peer past its byte cap is refused
        outright and reported below; it is never trimmed to fit and never re-homed onto a peer the hash did
        not rank.
      </p>

      <div className="mb-4 grid grid-cols-2 gap-3 text-xs md:grid-cols-4">
        <Fact label="Eligible peers" value={String(plan.membership.length)} />
        <Fact label="Fragments" value={`${plan.fragments_total} · ${fmtBytes(plan.fragment_bytes_total)}`} />
        <Fact label="Replicas placed" value={`${plan.replicas_placed} / ${plan.replicas_wanted}`} />
        <Fact label="Held by browsers" value={fmtBytes(held)} />
        <Fact label="Cap per peer" value={fmtBytes(plan.params.max_bytes)} />
        <Fact label="Fragment size" value={fmtBytes(plan.params.fragment_bytes)} />
        <Fact label="Replication" value={`${plan.params.replication_factor}×`} />
        <Fact label="Stores sharded" value={String(plan.params.stores.length)} />
      </div>

      {plan.holdings.length > 0 ? (
        <Table>
          <thead><tr><Th>Browser node</Th><Th>Fragments</Th><Th>Held</Th><Th>Cap used</Th><Th>Endpoint</Th></tr></thead>
          <tbody>
            {plan.holdings.map((h) => (
              <tr key={h.endpoint_id}>
                <Td className="font-mono text-xs font-medium">{h.node_name}</Td>
                <Td className="tabular-nums">{h.fragments.length}</Td>
                <Td className="tabular-nums">{fmtBytes(h.held_bytes)}</Td>
                <Td className="tabular-nums">{h.max_bytes ? `${Math.round((h.held_bytes / h.max_bytes) * 100)}%` : "—"}</Td>
                <Td className="font-mono text-[11px] text-secondary">{h.endpoint_id.slice(0, 16)}…</Td>
              </tr>
            ))}
          </tbody>
        </Table>
      ) : (
        <p className="text-sm text-muted">
          {plan.params.enabled
            ? "No eligible browser peers online. Eligibility is derived server-side from a platform-admin session — a browser cannot claim it."
            : "Shard planning is off on this node (HIVE_BROWSER_SHARDS=0)."}
        </p>
      )}

      {plan.refusals.length > 0 && (
        <div className="mt-4">
          <div className="mb-1 text-xs font-medium text-fg">
            Refusals{plan.refusals_total > plan.refusals.length ? ` (showing ${plan.refusals.length} of ${plan.refusals_total})` : ""}
          </div>
          <ul className="flex flex-col gap-1 text-xs text-secondary">
            {plan.refusals.map((r, i) => (
              <li key={`${r.fragment}-${r.endpoint_id}-${i}`}>
                <span className="font-mono text-[11px] text-muted">{r.reason}</span> {r.detail}
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="mt-4 border-t border-border pt-3">
        <button onClick={() => setShowTrust((v) => !v)} className="text-xs font-medium text-secondary underline underline-offset-2">
          {showTrust ? "Hide" : "What this does and does not prove"}
        </button>
        {showTrust && (
          <div className="mt-2 flex flex-col gap-3 text-xs">
            <div><span className="font-medium text-fg">Model.</span> <span className="text-secondary">{trust.model}</span></div>
            <TrustList title="Proves" items={trust.proves} tone="text-secondary" />
            <TrustList title="Does NOT prove" items={trust.does_not_prove} tone="text-amber-600 dark:text-amber-500" />
            <TrustList title="Options for a real possession guarantee (none implemented)" items={trust.options_for_a_real_possession_guarantee} tone="text-secondary" />
            <div><span className="font-medium text-fg">Implemented here.</span> <span className="text-secondary">{trust.implemented_here}</span></div>
          </div>
        )}
      </div>
    </Card>
  );
}

function TrustList({ title, items, tone }: { title: string; items: string[]; tone: string }) {
  return (
    <div>
      <div className="mb-1 font-medium text-fg">{title}</div>
      <ul className={`flex list-disc flex-col gap-1 pl-4 ${tone}`}>
        {items.map((t, i) => <li key={i}>{t}</li>)}
      </ul>
    </div>
  );
}

function Fact({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-[11px] uppercase tracking-wide text-muted">{label}</span>
      <span className="font-medium tabular-nums text-fg">{value}</span>
    </div>
  );
}

function Stat({
  icon,
  label,
  value,
  hint,
}: {
  icon: React.ReactNode;
  label: string;
  value: React.ReactNode;
  /** Optional sub-line. Optional so every existing call site is unchanged. */
  hint?: React.ReactNode;
}) {
  return (
    <Card className="flex flex-col gap-1">
      <span className="flex items-center gap-1.5 text-xs font-medium uppercase tracking-wide text-muted">{icon}{label}</span>
      <span className="text-2xl font-semibold text-fg">{value}</span>
      {hint ? <span className="text-xs text-secondary">{hint}</span> : null}
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
  const { data: any } = usePoll<AnycastTable>("/v1/anycast", 10000);
  const { data: rl, refresh } = usePoll<RateLimitStats>("/v1/ratelimit", 15000);
  const [limit, setLimit] = useState("");
  const [windowS, setWindowS] = useState("");
  const [saveErr, setSaveErr] = useState("");

  async function save(enabled: boolean) {
    setSaveErr("");
    try {
      await apiSend("PUT", "/v1/ratelimit", {
        enabled,
        limit: Number(limit) || rl?.limit || 100,
        window_ms: (Number(windowS) || (rl ? rl.window_ms / 1000 : 10)) * 1000,
      });
      refresh();
    } catch (e) {
      setSaveErr(String(e));
    }
  }

  return (
    <div className="mb-6 grid grid-cols-1 gap-4 lg:grid-cols-[1fr_360px]">
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
                <Td className="font-mono text-xs">{n.is_self ? "0ms (local)" : formatLatency(n.latency_ms)}</Td>
                <Td>{n.healthy ? <Badge tone="green">healthy</Badge> : <Badge tone="red">down</Badge>}</Td>
                <Td>{(any?.serving?.[n.name] ?? 0) > 0 ? <Badge tone="green">● serving</Badge> : <span className="text-xs text-muted">standby</span>}</Td>
              </tr>
            ))}
          </tbody>
        </Table>
      </Card>

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
        {saveErr && <p className="mt-2 text-xs text-red-500">{saveErr}</p>}
      </Card>
    </div>
  );
}
