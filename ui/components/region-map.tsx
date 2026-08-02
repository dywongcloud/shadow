"use client";

import { useMemo } from "react";

/**
 * Inline equirectangular dotted world map for the Function Regions setting.
 *
 * The land dots are sampled from the OpenEdge region art into a fixed GW×GH
 * grid, and the selected-region markers are projected into that SAME grid via
 * lon/lat, so a marker always lands on the correct part of the map. Colors are
 * driven by Tailwind theme tokens (`text-*` / `dark:`), so the map background
 * and land follow the current light/dark UI theme instead of being hard-black.
 */

const GW = 100;
const GH = 62;
// 1 = land cell, 0 = sea. Row-major, GW*GH chars.
const LAND_BITS =
  "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000111110000000000000000000000000000000000000000000000000000000000000000000000000000000000000000011101111111111111110000000000000110001110000000000111000000000000000000000000000000000000000000001111110111111111111111000000000000111000011110000000111100000000000000000000000000000000000000000001111101111111111111111110000000000111000000011110000011100000000000000000000000000000000001100000000101111111110111111111110000011100000000000000111100000001111101111111100000000000000000111111000000111111111110111111111111000011110000000110000111110111101111111111111110000000000000000011111110000111101111111011111111111000000111000000111000011111111111111111111111111000000000000000011111111111111111111111100011111111100000000000000011010111111111111111111111111111000000000000000111111111111111111101111110001111111110000000000000001001111111111111111111111111111110000000000000011111111111111111111111111000111111110000000000000000100111111111111111111111111111111000000000000000111111111111111111111111110011111111000000000000000011011111111111111111111111111110000000000000000111111111111111111111111111000111111100000000011110111111111111111111111111111111110000000000000000001111111111111111111111111110111111000000000011111111111111111111111111111111111111100000000000000001110001111111111111111111111011111001110000011111111111111111111111111111111110001110000000000001111100000011111111111100001110001110000110000011111111111111111111111111111111111000111110000000001111000000001111111111110001111000111000000000001111111111111111111111111111111111100001100000000000000000000000011111111111000111100000000000000000111111111111111111111111111111111111100110000000000000000000000001111111111111111111000000000000011001101111111111111111111111111111111111011000000000000000000000000011111111111111111110000000000011101111111111111111111111111111111111111101100000000000000000000000001111111111111111111000000000001111111111111111111111111111111111111111111100000000000000000000000001111111111111111101100000000000011111111111111111111111111111111111111101110000000000000000000000000111111111111111111000000000000001111111111111111111111111111111111111100100000000000000000000000000111111111111111101100000000000000111111111111111111111111111111111111110010000000000000000000000000011111111111111110000000000000000111111111111111111111111111111111111011001000000000000000000000000000111111111111110000000000000000011101111111111111111111111111111111110111100000000000000000000000000001111111111110000000000000000000111111100011111111111111111111111111011100000000000000000000000000000111111111110000000000000000000111111111101111111111111111111111111111100000000000000000000000000000011111110011000000000000000000111111111111111111111111111111111111110000000000000000000011000000000001111110000111000000000000000111111111111111111111111111111111111111100000000000000000001110000000000011111011111100000000000000111111111111111111111110111111111111111110000000000000000000010000000000000011111111111100000000000011111111111111111111111001111110111111001000000000000000000000000000000000000111111000000000000000001111111111111111111110000001100000111100110000000000000000000000000000000000000011101100000000000000111111111111111111101000000110000011110011000000000000000000000000000000000000000111111110000000000001111111111111111111100000001100001111000110000000000000000000000000000000000000000111111111000000000001110111111111111100000000000001110011100000000000000000000000000000000000000100011111111100000000000000001111111111100000000000000111011101110000000000000000000000000000000000010001111111111100000000000000111111111100000000000000001111111111110000000000000000000000000000000000000111111111111110000000000001111111110000000000000000011110110011111111000000000000000000000000000000011111111111111000000000000111111111000000000000000000111111000111101110000000000000000000000000000001111111111111000000000000011111111100000000000000000000011101101110001000000000000000000000000000000011111111111000000000000001111111111110000000000000000000011111110000010011000000000000000000000000000111111111100000000000000111111110111000000000000000000011111111000001101100000000000000000000000000001111111110000000000000011111110111000000000000000000111111111100001100000000000000000000000000000000111111100000000000000000111111001100000000000000000011111111111000110000000000000000000000000000000011111110000000000000000011111000000000000000000000001111111111110000000000000000000000000000000000001111110000000000000000000111000000000000000000000000111111111111000000000000000000000000000000000000111111000000000000000000011100000000000000000000000011111111111000000000000000000000000000000000000111110000000000000000000000000000000000000000000000000000001111000000000000000000000000000000000000001111000000000000000000000000000000000000000000000000000000011100000010000000000000000000000000000000111000000000000000000000000000000000000000000000000000000001110000001000000000000000000000000000000011100000000000000000000000000000000000000000000000000000000110000001100000000000000000000000000000001110000000000000000000000000000000000000000000000000000000011000001110000000000000000000000000000000110010000000000000000000000000000000000000000000000000000000000001110000000000000000000000000000000011101000000000000000000000000000000000000000000000000000000000000110000000000000000000000000000000000111000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

// Geographic window this grid covers (tuned to the source map art).
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
  /** Marker color; defaults to emerald. Pass distinct colors to tell co-located
   *  nodes apart. GPU nodes should pass `GPU_COLOR` (see `isGpu`) rather than a
   *  round-robin palette slot. */
  color?: string;
  /** GPU-bearing node (`gpu_count > 0`). Purely informational here — the
   *  caller is expected to have already set `color` to `GPU_COLOR` — but kept
   *  on the marker so the map can render GPU-specific affordances without
   *  re-deriving it from color equality. */
  isGpu?: boolean;
  /** This node advertises a `relay_url` — rendered as a small relay badge so
   *  the constellation shows which nodes provide mesh relay service. */
  hasRelay?: boolean;
  /** This node's GuardianDB client has a live `guardian_iroh_addr` (and the
   *  node itself is healthy) — rendered as a small DB/guardian badge. */
  hasGuardian?: boolean;
}

/** Fixed, unambiguous color for GPU-bearing nodes (`gpu_count > 0`) — never
 *  assigned round-robin, so blue reads as "this node has a GPU" fleet-wide,
 *  on both the map marker and any table/list color swatch (see regions page). */
export const GPU_COLOR = "#3b82f6";

/** Distinct, high-contrast palette so co-located non-GPU nodes are visually
 *  separable. Deliberately excludes `GPU_COLOR`'s blue so hue alone tells GPU
 *  nodes apart from everything else — a neutral node never lands on blue by
 *  round-robin chance. */
export const PALETTE = [
  "#10b981", "#f59e0b", "#ef4444", "#a855f7", "#06b6d4",
  "#ec4899", "#84cc16", "#f97316", "#14b8a6", "#6366f1",
];

/** Capability badge colors — distinct from both `GPU_COLOR` and `PALETTE` so
 *  they read as an overlay annotation, not a competing node color. */
const RELAY_BADGE_COLOR = "#a855f7";
const GUARDIAN_BADGE_COLOR = "#f59e0b";

/** A low-trust browser peer (bn-ui-constellation-satellites): rendered smaller
 *  and with a visually distinct glyph from every trusted fleet marker above —
 *  never a color from `PALETTE`/`GPU_COLOR`, so "satellite" reads unambiguously
 *  at a glance and is never mistaken for platform infrastructure. Positioned
 *  from a SEPARATE coarse presence feed (never `NodeInfo`) at a deliberately
 *  coarser precision than fleet markers. */
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

const SATELLITE_ONLINE_COLOR = "#38bdf8"; // sky-400 — distinct from every fleet/badge color above
const SATELLITE_DEGRADED_COLOR = "#94a3b8"; // slate-400 — visually recedes vs. a live fleet node

function satelliteColor(state?: string): string {
  return state === "degraded" || state === "suspended" ? SATELLITE_DEGRADED_COLOR : SATELLITE_ONLINE_COLOR;
}

/** Same co-location fan-out as fleet markers (see `RegionMap`'s `placed`
 *  memo), factored out so satellites — quantized to a much coarser grid and
 *  therefore far more likely to collide — get identical treatment without
 *  being mixed into the fleet markers' own dedup pass. */
function fanOutCollisions<T extends { x: number; y: number }>(points: T[], radius: number): T[] {
  const groups = new Map<string, T[]>();
  for (const p of points) {
    const key = `${p.x.toFixed(1)}_${p.y.toFixed(1)}`;
    (groups.get(key) ?? groups.set(key, []).get(key)!).push(p);
  }
  const out: T[] = [];
  for (const group of groups.values()) {
    if (group.length === 1) {
      out.push(group[0]);
      continue;
    }
    group.forEach((p, i) => {
      const angle = (2 * Math.PI * i) / group.length;
      out.push({ ...p, x: p.x + radius * Math.cos(angle), y: p.y + radius * Math.sin(angle) });
    });
  }
  return out;
}

/**
 * Inline equirectangular world map. Markers are placed at their ACTUAL lat/lon
 * (no hard-coded region table). Markers that land on the same spot — e.g. two
 * nodes running on the same machine, reporting identical coordinates — are fanned
 * out in a small ring and given distinct colors so each is individually visible.
 */
/** Hard cap on rendered satellites — a real bound, not a silent one: beyond
 *  this the map would spend more SVG nodes on browser peers than on the
 *  entire rest of the constellation. Clustering the overflow into a single
 *  "+N more" glyph per cell is real future work (bn-ui-constellation-
 *  performance); today the overflow is simply not drawn, and the caller-
 *  visible count (`placedSatellites.length` via the returned aria-label) is
 *  always the true total, so a capped render is distinguishable from "there
 *  are no more than this".*/
const MAX_RENDERED_SATELLITES = 300;

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
    // Project, then group by rounded coordinate to detect co-located markers.
    const proj = markers
      .filter((m) => Number.isFinite(m.lat) && Number.isFinite(m.lon))
      .map((m, i) => {
        const [x, y] = project(m.lon, m.lat);
        const color =
          m.color || (m.isGpu ? GPU_COLOR : autoColor ? PALETTE[i % PALETTE.length] : "#10b981");
        return { ...m, x, y, color };
      });
    const groups = new Map<string, typeof proj>();
    for (const p of proj) {
      const key = `${p.x.toFixed(1)}_${p.y.toFixed(1)}`;
      (groups.get(key) ?? groups.set(key, []).get(key)!).push(p);
    }
    const out: typeof proj = [];
    for (const group of groups.values()) {
      if (group.length === 1) {
        out.push(group[0]);
        continue;
      }
      // Fan co-located markers out around a small ring so each is visible.
      const radius = 1.6;
      group.forEach((p, i) => {
        const angle = (2 * Math.PI * i) / group.length;
        out.push({ ...p, x: p.x + radius * Math.cos(angle), y: p.y + radius * Math.sin(angle) });
      });
    }
    return out;
  }, [markers, autoColor]);

  const placedSatellites = useMemo(() => {
    const proj = satellites
      .filter((s) => Number.isFinite(s.lat) && Number.isFinite(s.lon))
      .map((s) => {
        const [x, y] = project(s.lon, s.lat);
        return { ...s, x, y, color: satelliteColor(s.state) };
      });
    // A slightly larger fan-out radius than fleet markers: satellites are
    // quantized to a much coarser grid (~55km cells), so real collisions —
    // not just visual near-misses — are the common case, not the exception.
    return fanOutCollisions(proj, 2.2).slice(0, MAX_RENDERED_SATELLITES);
  }, [satellites]);
  // bn-ui-constellation-performance's measurement gate, done: real CDP
  // profiling (real SVG DOM construction + a forced layout/paint flush, the
  // EXACT per-satellite <g><circle><rect><title> shape below, at n=50/300/
  // 1000/3000/10000) showed rendering is NOT the bottleneck at any realistic
  // scale -- ~2ms total at the current 300 cap, still only ~5ms even at
  // 10,000 raw input satellites (the fanOutCollisions pass scales with the
  // UNCAPPED input count since .slice happens after it, a real but currently
  // inconsequential inefficiency at real-world scale -- not reordered here,
  // since doing so would change WHICH points survive the cap in a way that
  // needs its own design decision, not a blind reorder chasing an unmeasured
  // problem). So the actual remaining gap was never performance -- it was
  // that a capped render was previously silent: the aria-label already
  // reported the true total, but nothing ON the map told a sighted user
  // their satellite count didn't match what's drawn. This is that.
  const satelliteOverflow = Math.max(0, satellites.length - placedSatellites.length);

  return (
    <svg
      viewBox={`0 0 ${GW} ${GH}`}
      className="block w-full text-slate-300 dark:text-slate-700"
      role="img"
      aria-label={`Region map — ${placed.length} node${placed.length === 1 ? "" : "s"}, ${satellites.length} browser node${satellites.length === 1 ? "" : "s"}`}
    >
      {/* Land texture — follows the theme via currentColor. */}
      {landDots.map((d, i) => (
        <circle key={i} cx={d.x} cy={d.y} r={0.34} fill="currentColor" />
      ))}
      {/* Geo markers, placed at real coordinates. */}
      {placed.map((m, i) => (
        <g key={`${m.id}-${i}`}>
          <circle cx={m.x} cy={m.y} r={2.4} fill={m.color} opacity={0.2} className="animate-pulse" />
          <circle cx={m.x} cy={m.y} r={1.3} fill={m.color} opacity={0.35} />
          {/* GPU nodes get an extra ring beyond the fixed blue fill, so the
              "this is a GPU node" signal survives even where fill color is
              hard to judge (small marker, dense cluster). */}
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
          {/* Per-node mesh-service badges — small offset dots so a glance at
              the constellation shows which nodes provide relay / GuardianDB,
              not just where they are located. */}
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
      {/* Browser-node satellites — deliberately smaller, a diamond glyph (never
          a circle, so it can never be mistaken for a trusted fleet marker even
          at a glance), and animated only under prefers-reduced-motion:no-preference. */}
      {placedSatellites.map((s, i) => (
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
          <title>
            {(s.label || s.id) + " · browser node (low-trust)" + (s.state ? ` · ${s.state}` : "")}
          </title>
        </g>
      ))}
      {/* Honest overflow indicator: the aria-label above already reports the
          TRUE satellite count, but nothing previously told a sighted user
          their on-map count was capped. Placed in the bottom-right corner in
          MAP UNITS (not pixels) so it stays correctly positioned across any
          rendered size, matching every other element in this SVG. */}
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
  );
}
