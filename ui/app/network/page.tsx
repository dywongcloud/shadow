"use client";

import { useState } from "react";
import { Globe2, Server, Share2, ShieldCheck, Database, Network } from "lucide-react";
import { Card, Badge, Button, PageHeader, Table, Th, Td } from "@/components/ui";
import { apiSend, usePoll, type NodeInfo, type AnycastTable, type RateLimitStats } from "@/lib/api";
import { SATELLITE_ONLINE_COLOR, SATELLITE_DEGRADED_COLOR } from "@/components/region-map";
import type { BrowserPresence } from "@/lib/run-node-client";
import { timeAgo } from "@/lib/utils";

interface ClusterStatus { term: number; leader: string; is_leader: boolean; members: string[]; consensus: string }

export default function NetworkPage() {
  // Mesh membership/leadership change at gossip cadence (~5s) — 10s polling is
  // fully live for this page (each fetch also shares the per-path TTL cache).
  // The previous /v1/overview poll here was never read anywhere — deleted.
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 10000);
  const { data: cluster } = usePoll<ClusterStatus>("/v1/cluster", 10000);
  // Low-trust browser-node presence — a SEPARATE feed from `/v1/nodes`, never
  // merged into the fleet node list or capacity totals anywhere on this page
  // (same discipline as the /regions constellation satellites).
  const { data: presenceFeed } = usePoll<{ presence: BrowserPresence[] }>("/v1/browser/presence", 8000);
  const presence = presenceFeed?.presence ?? [];
  const regions = Array.from(new Set((nodes ?? []).map((n) => n.region))).sort();

  return (
    <div>
      <PageHeader
        title="Constellation"
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
            {/* Blue is the platform-wide "has a GPU" color (see GPU_COLOR in
                region-map.tsx). Listed here because a diagram that renders some
                cubes blue with no legend entry is just an unexplained color. */}
            <span className="flex items-center gap-1.5"><span className="inline-block h-2.5 w-2.5 rotate-45 rounded-[2px] bg-[#3b82f6]" /> GPU</span>
            <span className="flex items-center gap-1.5"><span className="inline-block h-2.5 w-2.5 rotate-45 rounded-[2px] bg-[#9ca3af]" /> unhealthy</span>
            <span className="flex items-center gap-1.5" title="Low-trust volunteer browser peers — never counted as fleet capacity">
              <span className="inline-block h-2 w-2 rotate-45 rounded-[1px]" style={{ background: SATELLITE_ONLINE_COLOR }} /> browser node
            </span>
          </div>
        </div>
        <MeshDiagram nodes={nodes ?? []} presence={presence} />
        <p className="mt-2 text-center text-xs text-muted">
          Every node runs a gateway + Fluid pool, fully meshed over iroh QUIC. The function tunnel protocol
          rides those streams, so a gateway on one node can serve an instance on any other, anywhere reachable.
          {presence.length > 0 && (
            <> Orbiting satellites are admitted browser nodes — low-trust edge peers attached over the relay, never fleet capacity.</>
          )}
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
        <thead><tr><Th>Node</Th><Th>Region</Th><Th>Endpoint</Th><Th>vCPU</Th><Th>Memory</Th><Th>Disk</Th><Th>GPU</Th><Th>Role</Th><Th>Seen</Th></tr></thead>
        <tbody>
          {(nodes ?? []).map((n) => (
            <tr key={n.id}>
              <Td className="font-medium">{n.name}</Td>
              <Td><Badge tone="blue">{n.region}</Badge></Td>
              {/* Endpoint is operator-only; the sanitized tenant payload omits it. */}
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
 * node a 3D green isometric cube with its name below. Admitted browser nodes
 * (the separate low-trust presence feed) orbit further out as small diamonds,
 * anchored beside their relay fleet node when identifiable. Pure SVG,
 * theme-aware, no deps.
 */
function MeshDiagram({ nodes, presence }: { nodes: NodeInfo[]; presence: BrowserPresence[] }) {
  const W = 760;
  const H = 560;
  const cx = W / 2;
  const cy = H / 2;
  const R = Math.min(W, H) * 0.36;
  const n = nodes.length;

  // Only genuinely empty when there is NOTHING to draw. This used to return on
  // `n === 0` alone, which silently gated every browser satellite behind the
  // FLEET node list: any pass where /v1/nodes was empty or still loading threw
  // away a perfectly good presence feed and rendered "Discovering nodes…", so
  // browser nodes could never appear on their own. Satellites are positioned
  // relative to the ring, not to any individual node, so they draw fine with
  // an empty fleet — `nodes.map` yields no positions and every satellite
  // simply falls into the unanchored ring below.
  if (n === 0 && presence.length === 0) {
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

  // Browser-node satellites: admitted low-trust edge peers from the SEPARATE
  // presence feed, rendered as small diamonds on an outer orbit — never cubes,
  // never counted as fleet nodes or capacity. Unlike the geographic /regions
  // map there is no location-sharing requirement here: this diagram is
  // topological, so a browser that declined geo sharing is still a member of
  // the p2p network and still gets a satellite. Placement is deterministic
  // (stable lexical endpoint_id order) so a presence refresh doesn't reshuffle
  // the ring: a satellite anchors beside the fleet node whose relay it is
  // attached through when that relay is identifiable from relay_hint, fans out
  // in a small arc when several share one relay, and otherwise distributes
  // evenly around the ring. Rendered count is honestly capped.
  const SAT_ORBIT = R + 62;
  const MAX_SATELLITES = 150;
  const sortedPresence = [...presence].sort((a, b) => (a.endpoint_id < b.endpoint_id ? -1 : a.endpoint_id > b.endpoint_id ? 1 : 0));
  const satellites = sortedPresence.slice(0, MAX_SATELLITES);
  const satelliteOverflow = sortedPresence.length - satellites.length;
  // `relay_hint` carries the node's WHOLE connected relay set, comma-joined
  // (the worker publishes `status.relay`, which is `relays.join(",")`), so
  // feeding it straight to `new URL()` always threw and every satellite fell
  // into the unanchored bucket — the relay-anchoring below was dead code from
  // the day it landed. Take the first entry, and tolerate a bare hostname.
  const hostOf = (u?: string | null): string => {
    if (!u) return "";
    const first = u.split(",")[0]?.trim() ?? "";
    if (!first) return "";
    try { return new URL(first).hostname; } catch { /* fall through to bare-host */ }
    return /^[a-z0-9.-]+$/i.test(first) ? first : "";
  };
  const nodeAngle = (i: number) => -Math.PI / 2 + (i * 2 * Math.PI) / n;
  const anchorOf = satellites.map((p) => {
    const hintHost = hostOf(p.relay_hint);
    if (!hintHost) return -1;
    return nodes.findIndex((nd) => hostOf(nd.relay_url) === hintHost || (Boolean(nd.name) && hintHost.includes(nd.name)));
  });
  const anchoredTotal = new Map<number, number>();
  anchorOf.forEach((i) => { if (i >= 0) anchoredTotal.set(i, (anchoredTotal.get(i) ?? 0) + 1); });
  const anchoredSeen = new Map<number, number>();
  const unanchoredTotal = anchorOf.filter((i) => i < 0).length;
  let unanchoredSeen = 0;
  const satPos = satellites.map((p, k) => {
    let a: number;
    const ai = anchorOf[k];
    if (ai >= 0) {
      const seen = anchoredSeen.get(ai) ?? 0;
      anchoredSeen.set(ai, seen + 1);
      const m = anchoredTotal.get(ai) ?? 1;
      a = nodeAngle(ai) + (seen - (m - 1) / 2) * 0.14;
    } else {
      a = -Math.PI / 2 + ((unanchoredSeen++ + 0.5) * 2 * Math.PI) / Math.max(1, unanchoredTotal);
    }
    return { p, x: cx + SAT_ORBIT * Math.cos(a), y: cy + SAT_ORBIT * Math.sin(a) };
  });

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="mx-auto block w-full" style={{ maxHeight: 540 }} preserveAspectRatio="xMidYMid meet">
      {/*
        Wire animation lives in an inline <style> rather than a Tailwind class or
        styled-jsx: the keyframes have to travel with this SVG (it is the only
        consumer), and `stroke-dashoffset` is not an animatable Tailwind utility.
        The dash period is 6+6=12, so the offset animates by -24 — an exact
        multiple — which is what makes the loop seamless instead of visibly
        jumping on every restart. Motion is disabled under
        `prefers-reduced-motion`, where a continuously-crawling full-mesh graph
        is exactly the kind of thing that triggers discomfort.
      */}
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
      {/* mesh wires — blue (light) / white (dark), dashed and flowing */}
      <g fill="none" className="stroke-[#2f7fea] dark:stroke-white" strokeWidth={1.1} strokeOpacity={0.55} strokeLinecap="round">
        {edges.map((d, i) => (
          // Negative, per-edge staggered delay: every wire starts mid-cycle at a
          // different phase, so the mesh reads as many independent links rather
          // than one rigid pulse marching in lockstep. Negative (not positive)
          // so the stagger is already in effect on first paint — a positive
          // delay would show every wire frozen at phase 0 for up to a full
          // cycle before anything moved.
          <path key={i} d={d} className="hive-mesh-wire" style={{ animationDelay: `${-((i * 0.13) % 1.8).toFixed(2)}s` }} />
        ))}
      </g>
      {/* nodes */}
      {nodes.map((node, i) => (
        <g key={node.id} transform={`translate(${pos[i].x.toFixed(1)} ${pos[i].y.toFixed(1)})`}>
          <CubeIcon self={!!node.is_self} healthy={node.healthy !== false} gpu={(node.gpu_count ?? 0) > 0} />
          <text y={38} textAnchor="middle" style={{ fontSize: 13 }} className="fill-neutral-700 dark:fill-neutral-200">
            {node.name}
          </text>
        </g>
      ))}
      {/* browser-node satellites — small diamonds on an outer orbit, in the
          shared low-trust hues exported from region-map.tsx, with a hover
          title carrying label + state; degraded/suspended reads as slate. */}
      {satPos.map(({ p, x, y }) => (
        <g key={p.endpoint_id} transform={`translate(${x.toFixed(1)} ${y.toFixed(1)})`}>
          <rect
            x={-4}
            y={-4}
            width={8}
            height={8}
            rx={1}
            transform="rotate(45)"
            fill={p.state === "degraded" || p.state === "suspended" ? SATELLITE_DEGRADED_COLOR : SATELLITE_ONLINE_COLOR}
            fillOpacity={0.9}
          />
          <title>{`${p.display_label || p.endpoint_id} · browser node (low-trust)${p.state ? ` · ${p.state}` : ""}`}</title>
        </g>
      ))}
      {satelliteOverflow > 0 && (
        <text x={W - 8} y={H - 10} textAnchor="end" style={{ fontSize: 11 }} className="fill-neutral-500 dark:fill-neutral-400">
          {`+${satelliteOverflow} more browser node${satelliteOverflow === 1 ? "" : "s"} not drawn`}
        </text>
      )}
    </svg>
  );
}

/**
 * A 3D isometric cube node icon, centered at (0,0): BLUE for a GPU-bearing node
 * (`gpu_count > 0`), green otherwise, gray when unhealthy.
 *
 * Blue specifically, and not some other accent, because the platform already
 * fixes blue as "this node has a GPU" fleet-wide — see `GPU_COLOR` in
 * `components/region-map.tsx`, whose palette deliberately excludes blue so a
 * non-GPU node can never land on it by round-robin chance. Reusing that exact
 * hue here keeps one meaning for one color across the region map and this mesh
 * diagram; picking an independent blue would let the two drift apart.
 *
 * Health still wins over capability: an unhealthy GPU node renders gray, since
 * "can I use this node at all" is the more urgent signal than what it carries.
 */
function CubeIcon({ self, healthy, gpu }: { self: boolean; healthy: boolean; gpu?: boolean }) {
  const r = 18;
  // Face shades run light (top) → mid (left) → dark (right) to read as a lit
  // solid; the blue triple mirrors the green's relative luminance steps so the
  // two cube types look like the same object in two colors, not two shapes.
  const top = !healthy ? "#b0b6bd" : gpu ? "#60a5fa" : "#3ad15f";
  const left = !healthy ? "#8a9098" : gpu ? "#3b82f6" : "#23a64e";
  const right = !healthy ? "#6c727a" : gpu ? "#1d4ed8" : "#178a3d";
  return (
    <g>
      {self && (
        <circle
          r={r + 9}
          fill="none"
          stroke={gpu ? "#3b82f6" : "#34c759"}
          strokeOpacity={0.55}
          strokeWidth={1.5}
          strokeDasharray="3 3"
        />
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
  // Anycast/ratelimit change at gossip cadence / via this page's own mutations
  // (which invalidate the cache + refresh()) — slow polls lose nothing.
  const { data: any } = usePoll<AnycastTable>("/v1/anycast", 10000);
  const { data: rl, refresh } = usePoll<RateLimitStats>("/v1/ratelimit", 15000);
  const [limit, setLimit] = useState("");
  const [windowS, setWindowS] = useState("");
  const [saveErr, setSaveErr] = useState("");

  async function save(enabled: boolean) {
    // This is a node-wide (not per-tenant) safety control, so the backend
    // restricts it to platform operators — surface a clear message instead of
    // a silently-swallowed console error for anyone else.
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
        {saveErr && <p className="mt-2 text-xs text-red-500">{saveErr}</p>}
      </Card>
    </div>
  );
}
