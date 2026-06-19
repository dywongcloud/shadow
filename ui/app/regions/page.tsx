"use client";

import { Card, Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { usePoll, type NodeInfo } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { RegionMap, type MapMarker } from "@/components/region-map";

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

const PALETTE = [
  "#10b981", "#3b82f6", "#f59e0b", "#ef4444", "#a855f7",
  "#06b6d4", "#ec4899", "#84cc16", "#f97316", "#14b8a6",
];

export default function RegionsPage() {
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 3000);
  const list = nodes ?? [];
  const regions = Array.from(new Set(list.map((n) => n.region))).sort();

  // One colored marker per node, placed at its REAL reported coordinates. Nodes
  // sharing a location are fanned out + uniquely colored by the map component.
  const markers: MapMarker[] = list
    .filter((n) => typeof n.lat === "number" && typeof n.lon === "number")
    .map((n, i) => ({
      id: n.id,
      lat: n.lat as number,
      lon: n.lon as number,
      label: `${n.name} — ${n.city ?? n.region}`,
      color: PALETTE[i % PALETTE.length],
    }));

  // Color lookup so the table swatches match the map markers.
  const colorByNode: Record<string, string> = {};
  list.forEach((n, i) => { colorByNode[n.id] = PALETTE[i % PALETTE.length]; });

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
        </div>
      </Card>

      <Table>
        <thead><tr><Th>Node</Th><Th>Region</Th><Th>Location</Th><Th>Continent</Th><Th>Role</Th><Th>Seen</Th></tr></thead>
        <tbody>
          {list.map((n) => {
            const loc = n.city ? `${n.city}${n.country ? `, ${n.country}` : ""}` : "—";
            const continent = typeof n.lat === "number" && typeof n.lon === "number"
              ? continentOf(n.lat, n.lon) : "Unknown";
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
                <Td>{n.is_self ? <Badge tone="green">this node</Badge> : <Badge>peer</Badge>}</Td>
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
