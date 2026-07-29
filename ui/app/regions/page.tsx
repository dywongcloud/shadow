"use client";

import { Cpu, Radio, Database } from "lucide-react";
import { Card, Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { usePoll, type NodeInfo } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { RegionMap, type MapMarker, PALETTE, GPU_COLOR } from "@/components/region-map";

// Coarse continent classifier — mirrors hive_edge::continent_of so the UI labels
// match what the backend assigns from a node's lat/lon.
function continentOf(lat: number, lon: number): string {
  if (lat < -60) return "Antarctica";
  if (lat < 0 && lon >= 110 && lon <= 180) return "Oceania";
  if (lat >= 36 && lat <= 72 && lon >= -25 && lon <= 60) return "Europe";
  if (lat >= -35 && lat < 36 && lon >= -20 && lon <= 52) return "Africa";
  if (lon > 52 && lon <= 180) return "Asia";
  if (lat < 13 && lon >= -82 && lon <= -34) return "South America";
  if (lon >= -170 && lon <= -34) return "North America";
  return "Other";
}

// A GPU node (gpu_count > 0) always reads as GPU_COLOR — never a round-robin
// palette slot — both on the map marker and the table color swatch, so blue
// means "this node has a GPU" fleet-wide. Non-GPU nodes keep the neutral
// round-robin palette (which deliberately excludes blue).
function isGpuNode(n: NodeInfo): boolean {
  return (n.gpu_count ?? 0) > 0;
}
function hasRelay(n: NodeInfo): boolean {
  return Boolean(n.relay_url && n.relay_url.trim());
}
// GuardianDB's own iroh address is only meaningful for a currently-healthy
// node — an address configured on a node that's down isn't a service you can
// actually reach right now.
function hasGuardian(n: NodeInfo): boolean {
  return Boolean(n.guardian_iroh_addr && n.guardian_iroh_addr.trim() && n.healthy !== false);
}
function nodeColor(n: NodeInfo, i: number): string {
  return isGpuNode(n) ? GPU_COLOR : PALETTE[i % PALETTE.length];
}

export default function RegionsPage() {
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 3000);
  const list = nodes ?? [];
  const regions = Array.from(new Set(list.map((n) => n.region))).sort();

  // One colored marker per node, placed at its REAL reported coordinates. Nodes
  // sharing a location are fanned out + uniquely colored by the map component.
  // GPU nodes are pinned to GPU_COLOR; capability flags drive the small
  // relay/GuardianDB badges so the constellation shows which nodes provide
  // which mesh services, not just where they are located.
  const markers: MapMarker[] = list
    .filter((n) => typeof n.lat === "number" && typeof n.lon === "number")
    .map((n, i) => ({
      id: n.id,
      lat: n.lat as number,
      lon: n.lon as number,
      label: `${n.name} — ${n.city ?? n.region}`,
      color: nodeColor(n, i),
      isGpu: isGpuNode(n),
      hasRelay: hasRelay(n),
      hasGuardian: hasGuardian(n),
    }));

  // Color lookup so the table swatches match the map markers.
  const colorByNode: Record<string, string> = {};
  list.forEach((n, i) => { colorByNode[n.id] = nodeColor(n, i); });

  return (
    <div>
      <PageHeader title="Regions" desc="Your nodes, meshed into one cloud — placed where they actually are" />

      <div className="mb-5 flex flex-wrap gap-2">
        {regions.map((r) => (
          <Badge key={r} tone="blue">◍ {r}</Badge>
        ))}
        {!regions.length && <span className="text-sm text-muted">discovering…</span>}
      </div>

      {/* Live geographic map of the mesh. */}
      <Card className="mb-5 p-0">
        <div className="relative overflow-hidden rounded-xl bg-slate-50 dark:bg-[#070b14]">
          <RegionMap markers={markers} autoColor />
          <div className="absolute bottom-2 left-3 flex items-center gap-1.5 text-[11px] text-secondary">
            <span className="flex h-1.5 w-1.5 animate-pulse rounded-full bg-emerald-400" />
            {markers.length} node{markers.length === 1 ? "" : "s"} live on the mesh
          </div>
          {/* Legend: what a marker's color/badges mean, so the constellation
              reads as "which nodes provide which mesh services" at a glance. */}
          <div className="absolute bottom-2 right-3 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-secondary">
            <span className="inline-flex items-center gap-1">
              <span className="h-2 w-2 rounded-full" style={{ background: GPU_COLOR }} />
              GPU
            </span>
            <span className="inline-flex items-center gap-1">
              <span className="h-1.5 w-1.5 rounded-full bg-purple-500" />
              Relay
            </span>
            <span className="inline-flex items-center gap-1">
              <span className="h-1.5 w-1.5 rounded-full bg-amber-500" />
              GuardianDB
            </span>
          </div>
        </div>
      </Card>

      <Table>
        <thead>
          <tr>
            <Th>Node</Th><Th>Region</Th><Th>Location</Th><Th>Continent</Th><Th>RTT</Th><Th>Role</Th><Th>Services</Th><Th>Seen</Th>
          </tr>
        </thead>
        <tbody>
          {list.map((n) => {
            const loc = n.city ? `${n.city}${n.country ? `, ${n.country}` : ""}` : "—";
            const continent = typeof n.lat === "number" && typeof n.lon === "number"
              ? continentOf(n.lat, n.lon) : "Unknown";
            const gpu = isGpuNode(n);
            const relay = hasRelay(n);
            const guardian = hasGuardian(n);
            // Region no longer implies locality: two "san-jose" nodes turned
            // out to live 65ms away in another datacenter, silently degrading
            // every same-region decision (pooling, placement). A same-region
            // peer with a cross-DC RTT gets flagged amber so that mislabel is
            // visible here instead of discovered from a 2-hour model load.
            const selfRegion = list.find((m) => m.is_self)?.region;
            const rtt = n.is_self ? 0 : (n.latency_ms ?? 0);
            const crossDcSameRegion = !n.is_self && n.region === selfRegion && rtt >= 20;
            return (
              <tr key={n.id}>
                <Td className="font-medium">
                  <span className="inline-flex items-center gap-2">
                    <span className="h-2.5 w-2.5 shrink-0 rounded-full" style={{ background: colorByNode[n.id] }} />
                    {n.name}
                  </span>
                </Td>
                <Td><Badge tone="blue">{n.region}</Badge></Td>
                <Td className="text-secondary">{loc}</Td>
                <Td className="text-secondary">{continent}</Td>
                <Td className="font-mono text-xs">
                  {n.is_self ? (
                    <span className="text-muted">0ms</span>
                  ) : (
                    <span
                      className={crossDcSameRegion ? "text-amber-500 dark:text-amber-400" : undefined}
                      title={crossDcSameRegion ? "Same region label but cross-datacenter RTT — region may be mislabeled" : undefined}
                    >
                      {rtt}ms{crossDcSameRegion ? " ⚠" : ""}
                    </span>
                  )}
                </Td>
                <Td>{n.is_self ? <Badge tone="green">this node</Badge> : <Badge>peer</Badge>}</Td>
                <Td>
                  <span className="inline-flex items-center gap-2">
                    {gpu && (
                      <span
                        title={`GPU${n.gpu_model ? `: ${n.gpu_model}` : ""}`}
                        className="text-blue-500 dark:text-blue-400"
                      >
                        <Cpu className="h-3.5 w-3.5" />
                      </span>
                    )}
                    {relay && (
                      <span title="Relay" className="text-purple-500 dark:text-purple-400">
                        <Radio className="h-3.5 w-3.5" />
                      </span>
                    )}
                    {guardian && (
                      <span title="GuardianDB" className="text-amber-500 dark:text-amber-400">
                        <Database className="h-3.5 w-3.5" />
                      </span>
                    )}
                    {!gpu && !relay && !guardian && <span className="text-muted">—</span>}
                  </span>
                </Td>
                <Td className="text-muted">{timeAgo(n.last_seen_ms)}</Td>
              </tr>
            );
          })}
          {!list.length && <tr><Td className="text-muted">No nodes.</Td></tr>}
        </tbody>
      </Table>

      <Card className="mt-4 text-sm text-muted">
        Add a node: run <code className="text-fg">hive-cloud --name node-c --peer http://&lt;this-mac-ip&gt;:8786</code> on
        another machine. Its region is auto-derived from its real location.
      </Card>
    </div>
  );
}
