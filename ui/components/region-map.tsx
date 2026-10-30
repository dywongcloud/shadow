"use client";

import { useMemo } from "react";

const GW = 100;
const GH = 62;
/** Row-major GW*GH land mask: "1" = land, "0" = sea. */
const LAND_BITS =
  "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000111110000000000000000000000000000000000000000000000000000000000000000000000000000000000000000011101111111111111110000000000000110001110000000000111000000000000000000000000000000000000000000001111110111111111111111000000000000111000011110000000111100000000000000000000000000000000000000000001111101111111111111111110000000000111000000011110000011100000000000000000000000000000000001100000000101111111110111111111110000011100000000000000111100000001111101111111100000000000000000111111000000111111111110111111111111000011110000000110000111110111101111111111111110000000000000000011111110000111101111111011111111111000000111000000111000011111111111111111111111111000000000000000011111111111111111111111100011111111100000000000000011010111111111111111111111111111000000000000000111111111111111111101111110001111111110000000000000001001111111111111111111111111111110000000000000011111111111111111111111111000111111110000000000000000100111111111111111111111111111111000000000000000111111111111111111111111110011111111000000000000000011011111111111111111111111111110000000000000000111111111111111111111111111000111111100000000011110111111111111111111111111111111110000000000000000001111111111111111111111111110111111000000000011111111111111111111111111111111111111100000000000000001110001111111111111111111111011111001110000011111111111111111111111111111111110001110000000000001111100000011111111111100001110001110000110000011111111111111111111111111111111111000111110000000001111000000001111111111110001111000111000000000001111111111111111111111111111111111100001100000000000000000000000011111111111000111100000000000000000111111111111111111111111111111111111100110000000000000000000000001111111111111111111000000000000011001101111111111111111111111111111111111011000000000000000000000000011111111111111111110000000000011101111111111111111111111111111111111111101100000000000000000000000001111111111111111111000000000001111111111111111111111111111111111111111111100000000000000000000000001111111111111111101100000000000011111111111111111111111111111111111111101110000000000000000000000000111111111111111111000000000000001111111111111111111111111111111111111100100000000000000000000000000111111111111111101100000000000000111111111111111111111111111111111111110010000000000000000000000000011111111111111110000000000000000111111111111111111111111111111111111011001000000000000000000000000000111111111111110000000000000000011101111111111111111111111111111111110111100000000000000000000000000001111111111110000000000000000000111111100011111111111111111111111111011100000000000000000000000000000111111111110000000000000000000111111111101111111111111111111111111111100000000000000000000000000000011111110011000000000000000000111111111111111111111111111111111111110000000000000000000011000000000001111110000111000000000000000111111111111111111111111111111111111111100000000000000000001110000000000011111011111100000000000000111111111111111111111110111111111111111110000000000000000000010000000000000011111111111100000000000011111111111111111111111001111110111111001000000000000000000000000000000000000111111000000000000000001111111111111111111110000001100000111100110000000000000000000000000000000000000011101100000000000000111111111111111111101000000110000011110011000000000000000000000000000000000000000111111110000000000001111111111111111111100000001100001111000110000000000000000000000000000000000000000111111111000000000001110111111111111100000000000001110011100000000000000000000000000000000000000100011111111100000000000000001111111111100000000000000111011101110000000000000000000000000000000000010001111111111100000000000000111111111100000000000000001111111111110000000000000000000000000000000000000111111111111110000000000001111111110000000000000000011110110011111111000000000000000000000000000000011111111111111000000000000111111111000000000000000000111111000111101110000000000000000000000000000001111111111111000000000000011111111100000000000000000000011101101110001000000000000000000000000000000011111111111000000000000001111111111110000000000000000000011111110000010011000000000000000000000000000111111111100000000000000111111110111000000000000000000011111111000001101100000000000000000000000000001111111110000000000000011111110111000000000000000000111111111100001100000000000000000000000000000000111111100000000000000000111111001100000000000000000011111111111000110000000000000000000000000000000011111110000000000000000011111000000000000000000000001111111111110000000000000000000000000000000000001111110000000000000000000111000000000000000000000000111111111111000000000000000000000000000000000000111111000000000000000000011100000000000000000000000011111111111000000000000000000000000000000000000111110000000000000000000000000000000000000000000000000000001111000000000000000000000000000000000000001111000000000000000000000000000000000000000000000000000000011100000010000000000000000000000000000000111000000000000000000000000000000000000000000000000000000001110000001000000000000000000000000000000011100000000000000000000000000000000000000000000000000000000110000001100000000000000000000000000000001110000000000000000000000000000000000000000000000000000000011000001110000000000000000000000000000000110010000000000000000000000000000000000000000000000000000000000001110000000000000000000000000000000011101000000000000000000000000000000000000000000000000000000000000110000000000000000000000000000000000111000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

const LON_W = -180;
const LON_E = 180;
const LAT_TOP = 84;
const LAT_BOT = -60;

function project(lon: number, lat: number): [number, number] {
  const x = ((lon - LON_W) / (LON_E - LON_W)) * GW;
  const y = ((LAT_TOP - lat) / (LAT_TOP - LAT_BOT)) * GH;
  return [x, y];
}

/** A point to plot on the map, positioned by its REAL geographic coordinates. */
export interface MapMarker {
  id: string;
  lat: number;
  lon: number;
  label?: string;
  /** Marker color (default emerald); GPU nodes pass `GPU_COLOR`. */
  color?: string;
  /** GPU-bearing node (`gpu_count > 0`); informational here, the caller sets `color` to `GPU_COLOR`. */
  isGpu?: boolean;
  /** This node advertises a `relay_url` — rendered as a small relay badge so
   *  the constellation shows which nodes provide mesh relay service. */
  hasRelay?: boolean;
  /** This node's GuardianDB client has a live `guardian_iroh_addr` (and the
   *  node itself is healthy) — rendered as a small DB/guardian badge. */
  hasGuardian?: boolean;
}

/** Fixed color for GPU-bearing nodes (`gpu_count > 0`), never assigned round-robin: blue reads as "has a GPU" fleet-wide
 *  (map marker, regions table swatch, mesh diagram). */
export const GPU_COLOR = "#3b82f6";

/** Distinct palette for co-located non-GPU nodes; deliberately excludes `GPU_COLOR`'s blue. */
export const PALETTE = [
  "#10b981", "#f59e0b", "#ef4444", "#a855f7", "#06b6d4",
  "#ec4899", "#84cc16", "#f97316", "#14b8a6", "#6366f1",
];

/** Capability badge colors — distinct from both `GPU_COLOR` and `PALETTE` so
 *  they read as an overlay annotation, not a competing node color. */
const RELAY_BADGE_COLOR = "#a855f7";
const GUARDIAN_BADGE_COLOR = "#f59e0b";

/** A low-trust browser peer: smaller, a distinct diamond glyph, never a `PALETTE`/`GPU_COLOR` hue, and positioned from the
 *  separate coarse presence feed (never `NodeInfo`) at a coarser precision than fleet markers. */
export interface SatelliteMarker {
  id: string;
  lat: number;
  lon: number;
  label?: string;
  /** "starting" | "online" | "degraded" | "suspended" — drives dot color. */
  state?: string;
  /** ms since this presence was last refreshed, for an honest age readout. */
  ageMs?: number;
}

/** Browser-peer hues, shared with the /network mesh diagram: one meaning per color fleet-wide. */
export const SATELLITE_ONLINE_COLOR = "#38bdf8";
export const SATELLITE_DEGRADED_COLOR = "#94a3b8";

function satelliteColor(state?: string): string {
  return state === "degraded" || state === "suspended" ? SATELLITE_DEGRADED_COLOR : SATELLITE_ONLINE_COLOR;
}

const MIN_FAN_RING_RADIUS = 0.35;
const NEAREST_CLUSTER_GAP_DIVISOR = 3;
const FLEET_FAN_RADIUS = 1.6;
const SATELLITE_FAN_RADIUS = 2.2;

/** Fans co-located markers out on a small ring (shared by fleet markers and the coarser-grid satellites); the ring never
 *  exceeds a third of the gap to the nearest other cluster, so a multi-node city cannot smear into its neighbour. */
function fanOutCollisions<T extends { x: number; y: number }>(points: T[], radius: number): T[] {
  const groups = new Map<string, T[]>();
  for (const p of points) {
    const key = `${p.x.toFixed(1)}_${p.y.toFixed(1)}`;
    (groups.get(key) ?? groups.set(key, []).get(key)!).push(p);
  }
  const centres = Array.from(groups.values(), (g) => ({ x: g[0].x, y: g[0].y }));
  const out: T[] = [];
  Array.from(groups.values()).forEach((group, gi) => {
    if (group.length === 1) {
      out.push(group[0]);
      return;
    }
    let nearest = Infinity;
    for (let i = 0; i < centres.length; i++) {
      if (i === gi) continue;
      const d = Math.hypot(centres[i].x - centres[gi].x, centres[i].y - centres[gi].y);
      if (d < nearest) nearest = d;
    }
    const r = Math.max(MIN_FAN_RING_RADIUS, Math.min(radius, nearest / NEAREST_CLUSTER_GAP_DIVISOR));
    group.forEach((p, i) => {
      const angle = (2 * Math.PI * i) / group.length;
      out.push({ ...p, x: p.x + r * Math.cos(angle), y: p.y + r * Math.sin(angle) });
    });
  });
  return out;
}

/** Hard cap on rendered satellites; the overflow is not drawn but is stated on the map, and the aria-label always carries
 *  the true total. */
const MAX_RENDERED_SATELLITES = 300;

/** Inline equirectangular world map: markers sit at their ACTUAL lat/lon (no region table) in the same GW*GH grid the land
 *  dots are sampled into, and markers landing on one spot are fanned out on a ring so each stays visible. */
export function RegionMap({
  markers,
  satellites = [],
  autoColor = false,
}: {
  markers: MapMarker[];
  satellites?: SatelliteMarker[];
  autoColor?: boolean;
}) {
  const landDots = useMemo(() => {
    const out: Array<{ x: number; y: number }> = [];
    for (let r = 0; r < GH; r++) {
      for (let c = 0; c < GW; c++) {
        if (LAND_BITS[r * GW + c] === "1") out.push({ x: c + 0.5, y: r + 0.5 });
      }
    }
    return out;
  }, []);

  const placed = useMemo(() => {
    const proj = markers
      .filter((m) => Number.isFinite(m.lat) && Number.isFinite(m.lon))
      .map((m, i) => {
        const [x, y] = project(m.lon, m.lat);
        const color =
          m.color || (m.isGpu ? GPU_COLOR : autoColor ? PALETTE[i % PALETTE.length] : "#10b981");
        return { ...m, x, y, color };
      });
    return fanOutCollisions(proj, FLEET_FAN_RADIUS);
  }, [markers, autoColor]);

  const placedSatellites = useMemo(() => {
    const proj = satellites
      .filter((s) => Number.isFinite(s.lat) && Number.isFinite(s.lon))
      .map((s) => {
        const [x, y] = project(s.lon, s.lat);
        return { ...s, x, y, color: satelliteColor(s.state) };
      });
    return fanOutCollisions(proj, SATELLITE_FAN_RADIUS).slice(0, MAX_RENDERED_SATELLITES);
  }, [satellites]);
  const satelliteOverflow = Math.max(0, satellites.length - placedSatellites.length);

  return (
    <>
    <svg
      viewBox={`0 0 ${GW} ${GH}`}
      className="block w-full text-slate-300 dark:text-slate-700"
      role="img"
      aria-label={`Region map — ${placed.length} node${placed.length === 1 ? "" : "s"}, ${satellites.length} browser node${satellites.length === 1 ? "" : "s"}`}
    >
      {landDots.map((d, i) => (
        <circle key={i} cx={d.x} cy={d.y} r={0.34} fill="currentColor" />
      ))}
      {placed.map((m, i) => (
        <g key={`${m.id}-${i}`}>
          <circle cx={m.x} cy={m.y} r={2.4} fill={m.color} opacity={0.2} className="animate-pulse" />
          <circle cx={m.x} cy={m.y} r={1.3} fill={m.color} opacity={0.35} />
          {m.isGpu && (
            <circle
              cx={m.x}
              cy={m.y}
              r={1.7}
              fill="none"
              stroke={GPU_COLOR}
              strokeWidth={0.18}
              strokeDasharray="0.5 0.4"
            />
          )}
          <circle cx={m.x} cy={m.y} r={0.9} fill={m.color} stroke="#ffffff" strokeWidth={0.16} />
          <title>{m.label || m.id}</title>
          {m.hasRelay && (
            <circle cx={m.x + 1.15} cy={m.y - 1.15} r={0.55} fill={RELAY_BADGE_COLOR} stroke="#ffffff" strokeWidth={0.14}>
              <title>Relay — {m.label || m.id}</title>
            </circle>
          )}
          {m.hasGuardian && (
            <circle
              cx={m.x - 1.15}
              cy={m.y - 1.15}
              r={0.55}
              fill={GUARDIAN_BADGE_COLOR}
              stroke="#ffffff"
              strokeWidth={0.14}
            >
              <title>GuardianDB — {m.label || m.id}</title>
            </circle>
          )}
        </g>
      ))}
      {placedSatellites.map((s, i) => {
        const degraded = s.state === "degraded" || s.state === "suspended";
        return (
        <g key={`sat-${s.id}-${i}`} opacity={0.9}>
          <circle cx={s.x} cy={s.y} r={1.1} fill={s.color} opacity={0.18} className="motion-safe:animate-pulse" />
          <rect
            x={s.x - 0.5}
            y={s.y - 0.5}
            width={1}
            height={1}
            fill={s.color}
            stroke="#ffffff"
            strokeWidth={0.12}
            transform={`rotate(45 ${s.x} ${s.y})`}
          />
          {degraded && (
            <circle
              cx={s.x}
              cy={s.y}
              r={1.4}
              fill="none"
              stroke={s.color}
              strokeWidth={0.15}
              strokeDasharray="0.35 0.3"
            />
          )}
          <title>
            {(s.label || s.id) + " · browser node (low-trust)" + (s.state ? ` · ${s.state}` : "")}
          </title>
        </g>
        );
      })}
      {satelliteOverflow > 0 && (
        <text
          x={GW - 1}
          y={GH - 1}
          textAnchor="end"
          fontSize={2.2}
          fill="currentColor"
          opacity={0.75}
          className="select-none"
        >
          +{satelliteOverflow} more browser node{satelliteOverflow === 1 ? "" : "s"}
        </text>
      )}
    </svg>
    {satellites.length > 0 && (
      <ul className="sr-only">
        {satellites.map((s, i) => (
          <li key={`sat-sr-${s.id}-${i}`}>
            {(s.label || s.id) + " · browser node (low-trust)" + (s.state ? ` · ${s.state}` : "")}
          </li>
        ))}
      </ul>
    )}
    </>
  );
}
