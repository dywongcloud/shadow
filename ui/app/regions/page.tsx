"use client";

import { Cpu, Radio, Database } from "lucide-react";
import { Card, Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { useEffect, useMemo, useState } from "react";
import { usePoll, type NodeInfo } from "@/lib/api";
import { timeAgo, cn } from "@/lib/utils";
import { type MapMarker, type SatelliteMarker, PALETTE, GPU_COLOR } from "@/components/region-map";
import { WorldChoropleth } from "@/components/world-choropleth";
import { RegionTvLayer, RegionTvStats } from "@/components/region-tv";
import { RegionsCommandBar } from "@/components/regions-command-bar";
import { useNodeEventLog } from "@/lib/node-event-log";
import type { BrowserPresence } from "@/lib/run-node-client";

/** Coarse continent classifier mirroring `hive_edge::continent_of`, so the UI labels match what the backend assigns from a node's lat/lon. */
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

/** A GPU node (`gpu_count > 0`) always uses `GPU_COLOR` (map marker and table swatch alike), so blue means "has a GPU" fleet-wide; other nodes use the
 *  round-robin palette, which excludes blue. */
function isGpuNode(n: NodeInfo): boolean {
  return (n.gpu_count ?? 0) > 0;
}
function hasRelay(n: NodeInfo): boolean {
  return Boolean(n.relay_url && n.relay_url.trim());
}
/** GuardianDB's iroh address counts only for a healthy node: an address configured on a node that is down is not a service reachable now. */
function hasGuardian(n: NodeInfo): boolean {
  return Boolean(n.guardian_iroh_addr && n.guardian_iroh_addr.trim() && n.healthy !== false);
}
function nodeColor(n: NodeInfo, i: number): string {
  return isGpuNode(n) ? GPU_COLOR : PALETTE[i % PALETTE.length];
}

/** "local" is the backend's fallback region when boot-time geolocation fails (`region_id_from_geo`): a display-only relabel, grouping, filtering and
 *  color lookup keep the raw value. */
const displayRegion = (r: string) => (r === "local" ? "Unknown location" : r);

/** Region does not imply locality: a same-region peer at or past this RTT lives in another datacenter and is flagged. */
const CROSS_DC_RTT_MS = 20;

export default function RegionsPage() {
  const { data: nodes } = usePoll<NodeInfo[]>("/v1/nodes", 3000);
  const { data: presenceFeed } = usePoll<{ presence: BrowserPresence[] }>("/v1/browser/presence", 8000);
  const list = nodes ?? [];
  const presence = presenceFeed?.presence ?? [];
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(id);
  }, []);
  const regions = Array.from(new Set(list.map((n) => n.region))).sort();

  const [query, setQuery] = useState("");
  const [activeRegion, setActiveRegion] = useState<string | null>(null);
  const [connectFlash, setConnectFlash] = useState(false);
  const [activated, setActivated] = useState(false);
  const scopedList = activeRegion ? list.filter((n) => n.region === activeRegion) : list;
  const scopedPresence = useMemo(
    () => (activeRegion ? [] : (presenceFeed?.presence ?? [])),
    [activeRegion, presenceFeed],
  );

  const eventLog = useNodeEventLog(nodes);

  const markers: MapMarker[] = scopedList
    .filter((n) => typeof n.lat === "number" && typeof n.lon === "number")
    .map((n, i) => ({
      id: n.id,
      lat: n.lat as number,
      lon: n.lon as number,
      label: `${n.name} — ${n.city ?? displayRegion(n.region)}`,
      color: nodeColor(n, i),
      isGpu: isGpuNode(n),
      hasRelay: hasRelay(n),
      hasGuardian: hasGuardian(n),
    }));

  const colorByNode: Record<string, string> = {};
  scopedList.forEach((n, i) => { colorByNode[n.id] = nodeColor(n, i); });

  const satellites: SatelliteMarker[] = scopedPresence
    .filter((p) => typeof p.lat === "number" && typeof p.lon === "number")
    .map((p) => ({
      id: p.endpoint_id,
      lat: p.lat as number,
      lon: p.lon as number,
      label: p.display_label,
      state: p.state,
      ageMs: p.located_ms ? now - p.located_ms : undefined,
    }));

  const highlightId = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return null;
    const node = scopedList.find(
      (n) =>
        n.name.toLowerCase().includes(q) ||
        n.id.toLowerCase().includes(q) ||
        (n.peer_id ?? "").toLowerCase().includes(q) ||
        (n.city ?? "").toLowerCase().includes(q) ||
        (n.country ?? "").toLowerCase().includes(q),
    );
    if (node) return node.id;
    const sat = scopedPresence.find(
      (p) => p.display_label.toLowerCase().includes(q) || p.endpoint_id.toLowerCase().includes(q),
    );
    return sat?.endpoint_id ?? null;
  }, [query, scopedList, scopedPresence]);

  function handleConnectClick() {
    document.getElementById("connect-a-node")?.scrollIntoView({ behavior: "smooth", block: "center" });
    setConnectFlash(true);
    window.setTimeout(() => setConnectFlash(false), 1600);
  }

  return (
    <div>
      <PageHeader title="Regions" desc="Your nodes, meshed into one cloud — placed where they actually are" />

      <div className="mb-5 flex flex-wrap gap-2">
        {regions.map((r) => (
          <Badge key={r} tone="blue">◍ {displayRegion(r)}</Badge>
        ))}
        {!regions.length && <span className="text-sm text-muted">discovering…</span>}
      </div>

      <Card className="mb-5 overflow-hidden p-0">
        <RegionsCommandBar
          query={query}
          onQueryChange={(v) => {
            setQuery(v);
            if (!v.trim()) setActivated(false);
          }}
          onSubmit={() => setActivated(true)}
          regions={regions.map((r) => ({ value: r, label: displayRegion(r) }))}
          activeRegion={activeRegion}
          onRegionSelect={setActiveRegion}
          onConnectClick={handleConnectClick}
        />
        <div className="relative overflow-hidden bg-white dark:bg-black">
          <WorldChoropleth markers={markers} satellites={satellites} highlightId={highlightId} activated={activated} />
          <RegionTvLayer liveRegions={regions} />
          <div className="absolute bottom-2 left-3 flex items-center gap-1.5 text-[11px] text-secondary">
            <span className="flex h-1.5 w-1.5 animate-pulse rounded-full bg-emerald-400" />
            {markers.length} node{markers.length === 1 ? "" : "s"} live on the mesh
            {satellites.length > 0 && (
              <span className="text-muted"> · {satellites.length} browser node{satellites.length === 1 ? "" : "s"}</span>
            )}
          </div>
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
            <span className="inline-flex items-center gap-1" title="Low-trust volunteer browser peers — never counted as fleet capacity">
              <span className="h-1.5 w-1.5 rotate-45 bg-sky-400" />
              Browser node
            </span>
          </div>
        </div>
        <RegionTvStats />
        <div className="max-h-40 overflow-y-auto border-t border-border px-4 py-2 font-mono text-[11px] leading-5 text-secondary">
          {eventLog.length === 0 && <div className="text-muted">No mesh events observed yet this session.</div>}
          {eventLog.map((e) => (
            <div key={e.key} className="flex gap-2">
              <span className="shrink-0 text-muted">{timeAgo(e.ts)}</span>
              <span
                className={cn(
                  e.tone === "join" && "text-emerald-600 dark:text-emerald-400",
                  e.tone === "leave" && "text-muted",
                  e.tone === "warn" && "text-amber-600 dark:text-amber-400",
                )}
              >
                {e.message}
              </span>
            </div>
          ))}
        </div>
      </Card>

      <Table>
        <thead>
          <tr>
            <Th>Node</Th><Th>Region</Th><Th>Location</Th><Th>Continent</Th><Th>RTT</Th><Th>Role</Th><Th>Services</Th><Th>Seen</Th>
          </tr>
        </thead>
        <tbody>
          {scopedList.map((n) => {
            const loc = n.city ? `${n.city}${n.country ? `, ${n.country}` : ""}` : "—";
            const continent = typeof n.lat === "number" && typeof n.lon === "number"
              ? continentOf(n.lat, n.lon) : "Unknown";
            const gpu = isGpuNode(n);
            const relay = hasRelay(n);
            const guardian = hasGuardian(n);
            const selfRegion = list.find((m) => m.is_self)?.region;
            const rtt = n.is_self ? 0 : (n.latency_ms ?? 0);
            const crossDcSameRegion = !n.is_self && n.region === selfRegion && rtt >= CROSS_DC_RTT_MS;
            return (
              <tr key={n.id}>
                <Td className="font-medium">
                  <span className="inline-flex items-center gap-2">
                    <span className="h-2.5 w-2.5 shrink-0 rounded-full" style={{ background: colorByNode[n.id] }} />
                    {n.name}
                  </span>
                </Td>
                <Td><Badge tone="blue">{displayRegion(n.region)}</Badge></Td>
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
          {!scopedList.length && (
            <tr><Td className="text-muted">{list.length ? `No nodes in ${displayRegion(activeRegion ?? "")}.` : "No nodes."}</Td></tr>
          )}
        </tbody>
      </Table>

      <Card
        id="connect-a-node"
        className={cn(
          "mt-4 text-sm text-muted transition-shadow",
          connectFlash && "ring-2 ring-accent",
        )}
      >
        Add a node: run <code className="text-fg">hive-cloud --name node-c --peer http://&lt;this-mac-ip&gt;:8786</code> on
        another machine. Its region is auto-derived from its real location.
      </Card>
    </div>
  );
}
